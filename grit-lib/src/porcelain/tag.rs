//! Tag creation, listing, deletion, verification and the list filter/sort helpers.
//!
//! The `grit tag` command parses argv, resolves identity, and formats output;
//! what lives here is presentation-free tag logic: creating lightweight and
//! annotated tags ([`create_tag`]), listing ([`list_tags`]), deleting
//! ([`delete_tag`]) and verifying ([`verify_tag_by_name`]) tag refs, filter/sort
//! predicates, and `-m`/`-F` message cleanup ([`strip_comments`]).
//!
//! # What this module owns
//!
//! - Tag creation ([`create_tag`], [`CreateTagRequest`], [`TagKind`]), listing
//!   ([`list_tags`], [`TagSummary`], [`tag_ref_metadata`]), deletion ([`delete_tag`])
//!   and verification by name ([`verify_tag_by_name`], [`VerifiedTag`]).
//! - Ref list reachability filters (`--contains`, `--merged`, `--points-at`) live in [`crate::refs::listing`].
//! - Ref list sort/filter helpers live in [`crate::refs::listing`] (`compare_version_names`,
//!   [`crate::refs::listing::pattern_matches_short_name`], [`creator_date`],
//!   [`crate::refs::listing::epoch_from_identity`], [`crate::refs::listing::ref_query_options_for_repo`]).
//! - Tag message cleanup: [`strip_comments`] then [`crate::porcelain::commit::normalize_commit_message`]
//!   via [`normalize_tag_message`].

use std::cell::OnceCell;
use std::sync::Arc;

use crate::check_ref_format::validate_tag_short_name;
use crate::commit_graph_file::CommitGraphChain;
use crate::commit_pretty::message_subject;
use crate::diff_rename_parallel::parallel_map_indices;
use crate::error::{Error, Result};
use crate::hash::hash_object;
use crate::ident::signature_timestamp_for_pretty;
use crate::objects::{parse_tag, serialize_tag, ObjectId, ObjectKind, TagData};
use crate::parallel::ParallelOp;
use crate::refs;
use crate::refs::listing::{
    creator_date as listing_creator_date, epoch_from_identity, query_refs,
    ref_query_options_for_repo, RefListing, RefNamespace, RefQuery, RefQueryError,
};
use crate::refs::PackedRefPeels;
use crate::repo::Repository;
use crate::signing::{parse_signed_buffer, should_sign_tag, sign_serialized_tag, GpgConfig};
use crate::tree_sha256::{
    parse_tree_sha256_tag_headers, verify_tag_data, TagTreeSha256Outcome, TagVerification,
    TagVerifyOptions, TreeSha256Claim, TreeSha256Options,
};
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
    /// `refs/tags/<name>` points directly at a non-tag object, so there is no
    /// annotated tag object to verify.
    #[error("tag '{name}' is lightweight")]
    Lightweight {
        /// Short tag name.
        name: String,
    },
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
    /// Raw tagger line from the tag object (`Name <email> <epoch> <tz>`).
    pub tagger_ident: String,
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

/// Amount of tag metadata requested from [`list_tags`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TagDetail {
    /// Return only short tag names without classifying their target objects.
    Names,
    /// Return tag kind, peeled commit, and target kind without annotation text.
    Kind,
    /// Return complete tag summaries, including annotated-tag message and tagger fields.
    #[default]
    Annotation,
}

/// Result rows returned by [`list_tags`] for the requested [`TagDetail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagList {
    /// Short tag names requested with [`TagDetail::Names`].
    Names(Vec<String>),
    /// Classified summaries requested with [`TagDetail::Kind`] or [`TagDetail::Annotation`].
    Summaries(Vec<TagSummary>),
}

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
    // An unreadable or dangling tag ref counts as absent, so it is overwritten.
    let replaced = match repo.references().resolve_if_exists(&tag_ref) {
        Ok(Some(existing)) => {
            if req.force {
                Some(existing)
            } else {
                return Err(Error::Tag(TagError::AlreadyExists {
                    name: req.name.clone(),
                }));
            }
        }
        Ok(None) | Err(_) => None,
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
        refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), &tag_ref, &ref_oid)?;
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

/// List complete tag summaries matching `query`.
///
/// This preserves the annotation-rich API. Use [`list_tags_with_detail`] when
/// names or classification alone are sufficient.
///
/// # Errors
///
/// Propagates ref-query, config, and object read failures.
pub fn list_tags(repo: &Repository, query: &RefQuery) -> Result<Vec<TagSummary>> {
    let TagList::Summaries(tags) = list_tags_with_detail(repo, query, TagDetail::Annotation)?
    else {
        return Err(Error::Message(
            "annotation detail returned tag names without summaries".into(),
        ));
    };
    Ok(tags)
}

/// List tags matching `query` (always scoped to [`RefNamespace::Tags`]).
///
/// [`TagDetail::Names`] avoids tag-object reads. [`TagDetail::Kind`] classifies
/// packed refs from their peel records and falls back to objects when needed.
/// [`TagDetail::Annotation`] reads annotated tag objects in parallel when the
/// thread policy allows.
/// Default CLI sort is `version`; pass [`RefQuery::sort`] explicitly in library callers.
///
/// # Errors
///
/// Propagates ref-query, config, and object read failures.
pub fn list_tags_with_detail(
    repo: &Repository,
    query: &RefQuery,
    detail: TagDetail,
) -> Result<TagList> {
    let mut query = query.clone();
    query.namespaces = vec![RefNamespace::Tags];
    let options = ref_query_options_for_repo(repo)?;
    let listings = query_refs(repo, &query, &options).map_err(ref_query_to_error)?;
    if listings.is_empty() {
        return Ok(match detail {
            TagDetail::Names => TagList::Names(Vec::new()),
            TagDetail::Kind | TagDetail::Annotation => TagList::Summaries(Vec::new()),
        });
    }
    match detail {
        TagDetail::Names => Ok(TagList::Names(
            listings.into_iter().map(|entry| entry.short_name).collect(),
        )),
        TagDetail::Kind => {
            let classifier = TagClassifier::new(repo);
            Ok(TagList::Summaries(
                listings
                    .into_iter()
                    .map(|entry| classifier.summarize(&entry))
                    .collect(),
            ))
        }
        TagDetail::Annotation => {
            let config = repo.config()?;
            let par = repo
                .thread_policy()
                .resolve(ParallelOp::DiffContent, config.as_ref());
            let odb = repo.odb();
            odb.with_pack_read_context(|| {
                parallel_map_indices(odb, listings.len(), par, listings.len() * 512, |i| {
                    summarize_listing(repo, &listings[i])
                })
                .map(TagList::Summaries)
            })
        }
    }
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
    let subject_message = match parse_signed_buffer(tag.message.as_bytes()) {
        Some((payload, _signature)) => String::from_utf8_lossy(&payload).into_owned(),
        None => tag.message.clone(),
    };
    Some(TagAnnotation {
        subject: message_subject(&subject_message),
        message: tag.message.clone(),
        tagger,
        tagger_ident: tagger_line.to_owned(),
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

/// Classifies tag refs for [`list_tags`], preferring the peel lines of `packed-refs`
/// over reading tag objects.
struct TagClassifier<'a> {
    repo: &'a Repository,
    packed: Option<PackedRefPeels>,
    graph: OnceCell<Option<Arc<CommitGraphChain>>>,
}

impl<'a> TagClassifier<'a> {
    fn new(repo: &'a Repository) -> Self {
        // A malformed `packed-refs` only loses the shortcut; classification falls back to
        // the objects.
        let packed = PackedRefPeels::for_store(repo.refs(), repo.git_dir()).ok();
        Self {
            repo,
            packed,
            graph: OnceCell::new(),
        }
    }

    fn summarize(&self, listing: &RefListing) -> TagSummary {
        let (kind, peeled, target_kind) = match self
            .packed
            .as_ref()
            .and_then(|packed| packed.peeled(&listing.full_name, &listing.oid))
        {
            Some(Some(target)) => {
                let target_kind = self.object_kind(&target).unwrap_or(ObjectKind::Blob);
                (
                    TagKind::Annotated,
                    (target_kind == ObjectKind::Commit).then_some(target),
                    target_kind,
                )
            }
            Some(None) => {
                let target_kind = self.object_kind(&listing.oid).unwrap_or(ObjectKind::Blob);
                (
                    TagKind::Lightweight,
                    (target_kind == ObjectKind::Commit).then_some(listing.oid),
                    target_kind,
                )
            }
            None => self.classify_from_objects(&listing.oid),
        };
        TagSummary {
            name: listing.short_name.clone(),
            oid: listing.oid,
            kind,
            peeled,
            target_kind,
            annotation: None,
        }
    }

    fn classify_from_objects(&self, oid: &ObjectId) -> (TagKind, Option<ObjectId>, ObjectKind) {
        let Some(kind) = self.object_kind(oid) else {
            return (TagKind::Lightweight, None, ObjectKind::Blob);
        };
        if kind != ObjectKind::Tag {
            return (
                TagKind::Lightweight,
                (kind == ObjectKind::Commit).then_some(*oid),
                kind,
            );
        }
        let (target, target_kind) = self
            .repo
            .odb()
            .peel_tags(*oid)
            .ok()
            .unwrap_or((*oid, ObjectKind::Blob));
        (
            TagKind::Annotated,
            (target_kind == ObjectKind::Commit).then_some(target),
            target_kind,
        )
    }

    fn object_kind(&self, oid: &ObjectId) -> Option<ObjectKind> {
        let graph = self.graph.get_or_init(|| {
            self.repo
                .caches()
                .commit_graph_chain(self.repo.odb().objects_dir())
        });
        if graph.as_ref().is_some_and(|g| g.find_commit(oid).is_some()) {
            return Some(ObjectKind::Commit);
        }
        self.repo.odb().read_info(oid).ok().map(|info| info.kind)
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

/// Result of [`verify_tag_by_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTag {
    /// Short tag name (without `refs/tags/`).
    pub name: String,
    /// Id of the annotated tag object stored at `refs/tags/<name>`.
    pub oid: ObjectId,
    /// Object named by the tag's `object` header.
    pub target: ObjectId,
    /// Signature and `tree-sha256` verdicts.
    pub verification: TagVerification,
}

/// Verify the annotated tag at `refs/tags/<name>`.
///
/// Resolves the tag through the repository's ref store, reads the tag object once,
/// and runs the same signature and `tree-sha256` checks as
/// [`Repository::verify_tag`].
///
/// # Errors
///
/// Returns [`TagError::InvalidName`] when `name` fails tag-name checks,
/// [`TagError::NotFound`] when the tag ref does not exist, and
/// [`TagError::Lightweight`] when the ref points at a non-tag object. Propagates
/// ref-store, object-read, tag-header parse and signing errors.
pub fn verify_tag_by_name(
    repo: &Repository,
    name: &str,
    options: &TagVerifyOptions,
) -> Result<VerifiedTag> {
    validate_tag_short_name(name).map_err(|e| {
        Error::Tag(TagError::InvalidName {
            detail: e.to_string(),
        })
    })?;
    let oid = repo
        .references()
        .resolve_if_exists(&format!("{TAGS_PREFIX}{name}"))?
        .ok_or_else(|| {
            Error::Tag(TagError::NotFound {
                name: name.to_owned(),
            })
        })?;
    let obj = repo.odb().read(&oid)?;
    if obj.kind != ObjectKind::Tag {
        return Err(Error::Tag(TagError::Lightweight {
            name: name.to_owned(),
        }));
    }
    let headers = parse_tree_sha256_tag_headers(&obj.data)?;
    let verification = verify_tag_data(repo, &obj.data, &headers, options)?;
    Ok(VerifiedTag {
        name: name.to_owned(),
        oid,
        target: headers.target,
        verification,
    })
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
            author: Some(
                "T <t@example.com> 1700000000 +0000"
                    .parse()
                    .expect("signature"),
            ),
            committer: Some(
                "T <t@example.com> 1700000000 +0000"
                    .parse()
                    .expect("signature"),
            ),
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
        assert_eq!(
            list_tags_with_detail(&repo, &name_sorted_query(), TagDetail::Names).unwrap(),
            TagList::Names(Vec::new())
        );
    }

    #[test]
    fn list_tags_detail_levels_are_typed_and_sorted() {
        let (_tmp, repo, commit) = repo_with_commit();
        let annotated = tag(&repo, "v2", commit, Some("release"));
        tag(&repo, "v1", commit, None);

        assert_eq!(
            list_tags_with_detail(&repo, &name_sorted_query(), TagDetail::Names).unwrap(),
            TagList::Names(vec!["v1".to_owned(), "v2".to_owned()])
        );

        let TagList::Summaries(tags) =
            list_tags_with_detail(&repo, &name_sorted_query(), TagDetail::Kind).unwrap()
        else {
            panic!("kind detail must return summaries");
        };
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "v1");
        assert_eq!(tags[0].oid, commit);
        assert_eq!(tags[0].kind, TagKind::Lightweight);
        assert_eq!(tags[0].target_kind, ObjectKind::Commit);
        assert_eq!(tags[1].name, "v2");
        assert_eq!(tags[1].oid, annotated);
        assert_eq!(tags[1].kind, TagKind::Annotated);
        assert!(tags[1].annotation.is_none());

        let TagList::Summaries(tags) =
            list_tags_with_detail(&repo, &name_sorted_query(), TagDetail::Annotation).unwrap()
        else {
            panic!("annotation detail must return summaries");
        };
        assert!(tags[1].annotation.is_some());
    }

    #[test]
    fn delete_tag_returns_old_target_and_keeps_tag_object() {
        let (_tmp, repo, commit) = repo_with_commit();
        let annotated = tag(&repo, "v1", commit, Some("release"));

        assert_eq!(delete_tag(&repo, "v1", false).unwrap(), annotated);
        assert_eq!(
            list_tags_with_detail(&repo, &name_sorted_query(), TagDetail::Names).unwrap(),
            TagList::Names(Vec::new())
        );
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

    fn unchecked_verify_options() -> TagVerifyOptions {
        TagVerifyOptions {
            check_signature: false,
            gpg: GpgConfig::from_config(&crate::config::ConfigSet::default()).unwrap(),
        }
    }

    #[test]
    fn verify_tag_by_name_reports_tag_oid_and_target() {
        let (_tmp, repo, commit) = repo_with_commit();
        let annotated = tag(&repo, "v1", commit, Some("release"));

        let verified = verify_tag_by_name(&repo, "v1", &unchecked_verify_options()).unwrap();

        assert_eq!(verified.name, "v1");
        assert_eq!(verified.oid, annotated);
        assert_eq!(verified.target, commit);
        assert_eq!(
            verified.verification,
            repo.verify_tag(annotated, &unchecked_verify_options())
                .unwrap()
        );
        assert!(verified.verification.is_ok());
    }

    #[test]
    fn verify_tag_by_name_lightweight_is_typed() {
        let (_tmp, repo, commit) = repo_with_commit();
        tag(&repo, "light", commit, None);
        let err = verify_tag_by_name(&repo, "light", &unchecked_verify_options()).unwrap_err();
        assert!(
            matches!(&err, Error::Tag(TagError::Lightweight { name }) if name == "light"),
            "{err:?}"
        );
    }

    #[test]
    fn verify_tag_by_name_missing_is_typed() {
        let (_tmp, repo, _) = repo_with_commit();
        let err = verify_tag_by_name(&repo, "nope", &unchecked_verify_options()).unwrap_err();
        assert!(
            matches!(&err, Error::Tag(TagError::NotFound { name }) if name == "nope"),
            "{err:?}"
        );
    }

    #[test]
    fn verify_tag_by_name_invalid_name_is_typed() {
        let (_tmp, repo, _) = repo_with_commit();
        let err = verify_tag_by_name(&repo, "bad..tag", &unchecked_verify_options()).unwrap_err();
        assert!(
            matches!(err, Error::Tag(TagError::InvalidName { .. })),
            "{err:?}"
        );
    }
}
