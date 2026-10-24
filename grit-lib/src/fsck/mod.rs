//! Repository-wide integrity checks (object store, connectivity, refs, MIDX, commit-graph).
//!
//! Validates loose and packed objects (in parallel when worthwhile), walks reachability from
//! [`reachability_roots`], optionally reports dangling objects, and runs ref/MIDX/graph checks.
//! Progress is reported through [`ProgressSink`]; the library never prints.
//!
//! ```rust
//! use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
//! use grit_lib::progress::{NeverCancel, NullProgress};
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let config = repo.config()?;
//! let report = fsck(
//!     &repo,
//!     &FsckOptions::connectivity_and_refs(false),
//!     &mut NullProgress,
//!     &NeverCancel,
//! )?;
//! assert!(report.ok(&config, &FsckOptions::connectivity_and_refs(false)));
//! # Ok(())
//! # }
//! ```

mod roots;

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use crate::commit_graph_file::CommitGraphChain;
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::fsck_standalone::{fsck_object, FsckError, FsckObjectOptions};
use crate::gitmodules::{DotFsckIssue, DotFsckTracker};
use crate::index::MODE_GITLINK;
use crate::midx::verify_midx;
use crate::objects::{
    oid_hash_map, oid_hash_set, oid_hash_set_from, parse_commit, parse_tag, parse_tree, HashAlgo,
    ObjectId, ObjectKind, OidHashMap, OidHashSet, TreeEntry, TreeEntryRef, TreeRefIter,
};
use crate::odb::Odb;
use crate::pack::{prepare_pack_for_object_fsck, PackFsckTrailerIssue};
use crate::pack_fsck_verify::{verify_pack_objects_offset_order, PackFsckObjectOutcome};
use crate::parallel::{try_par_map, ParallelOp, ParallelOptions};
use crate::progress::{Cancel, ProgressPhase, ProgressSink};
use crate::refs_fsck::{refs_fsck, RefsFsckIssue};
use crate::repo::Repository;

pub use roots::{index_object_roots, reachability_roots};

/// Controls which fsck phases run and how strictly diagnostics are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FsckOptions {
    /// When true, skip full object-store validation and only check connectivity from roots.
    pub connectivity_only: bool,
    /// When false, skip reachability walks and index blob checks (refs/MIDX may still run).
    pub check_connectivity: bool,
    /// When true, compute [`FsckReport::dangling`] after the reachability walk.
    pub check_dangling: bool,
    /// Treat ref-fsck warnings as errors (Git `--strict` for references).
    pub strict: bool,
}

impl Default for FsckOptions {
    fn default() -> Self {
        Self {
            connectivity_only: false,
            check_connectivity: true,
            check_dangling: true,
            strict: false,
        }
    }
}

impl FsckOptions {
    /// Full object validation, dangling detection, optional strict refs.
    #[must_use]
    pub fn full_repository(strict: bool) -> Self {
        Self {
            connectivity_only: false,
            check_connectivity: true,
            check_dangling: true,
            strict,
        }
    }

    /// Connectivity and refs only (no store-wide object fsck, no dangling list).
    #[must_use]
    pub fn connectivity_and_refs(strict: bool) -> Self {
        Self {
            connectivity_only: true,
            check_connectivity: true,
            check_dangling: false,
            strict,
        }
    }

    /// Copy with [`Self::check_dangling`] replaced.
    #[must_use]
    pub fn with_check_dangling(mut self, check_dangling: bool) -> Self {
        self.check_dangling = check_dangling;
        self
    }
}

/// Where a missing-object diagnostic was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingObjectReferrer {
    /// Staged in the index but absent from the object database.
    Index,
    /// Pack index lists the oid but the object bytes are absent.
    Pack,
    /// Another object referenced this oid during connectivity or tree walks.
    Object(ObjectId),
}

/// Human-readable detail for [`FsckIssue::MissingObject`].
#[must_use]
pub fn missing_object_detail(oid: ObjectId, referrer: MissingObjectReferrer) -> String {
    match referrer {
        MissingObjectReferrer::Index => {
            format!("object {} missing (staged in index)", oid.to_hex())
        }
        MissingObjectReferrer::Pack => format!("object {} missing from pack", oid.to_hex()),
        MissingObjectReferrer::Object(parent) if parent.is_zero() => {
            format!("object {} missing during reachability walk", oid.to_hex())
        }
        MissingObjectReferrer::Object(parent) => {
            format!("object {} referenced by {}", oid.to_hex(), parent.to_hex())
        }
    }
}

/// One repository fsck finding with a stable, typed shape.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FsckIssue {
    /// Loose object file could not be read or decompressed.
    CorruptLoose {
        /// Expected object id from the path.
        oid: ObjectId,
        /// Path to the loose object file.
        path: PathBuf,
    },
    /// Loose file content hashes to a different id than its path.
    HashMismatch {
        /// Id encoded in the path.
        oid: ObjectId,
        /// Loose object path.
        path: PathBuf,
    },
    /// Standalone object buffer fsck failed.
    BadObject {
        /// Object id.
        oid: ObjectId,
        /// Parsed fsck error (Git-compatible msg id in [`FsckError::id`]).
        error: FsckError,
    },
    /// Pack trailing checksum or structure check failed.
    PackChecksum {
        /// Path to the `.pack` file.
        pack: PathBuf,
    },
    /// Pack checksum embedded in the `.idx` does not match the pack trailer.
    IdxMismatch {
        /// Path to the `.pack` file (index is alongside with `.idx`).
        pack: PathBuf,
    },
    /// Reachability walk found a missing object.
    MissingObject {
        /// Missing object id.
        oid: ObjectId,
        /// Where the missing oid was discovered.
        referrer: MissingObjectReferrer,
    },
    /// Tree entry points at an object of the wrong kind (or an invalid link).
    BrokenLink {
        /// Tree object containing the bad entry.
        tree: ObjectId,
        /// Entry path component (UTF-8 lossy).
        name: String,
        /// Mode from the tree entry.
        mode: u32,
        /// Target oid from the tree entry.
        target: ObjectId,
    },
    /// Reference database problem (symref, packed-refs, etc.).
    BadRef(RefsFsckIssue),
    /// Multi-pack-index verification failed.
    MidxInvalid {
        /// Human-readable detail strings from [`verify_midx`].
        details: Vec<String>,
    },
    /// Commit-graph file is present but invalid.
    CommitGraphInvalid {
        /// Error detail.
        detail: String,
    },
}

/// Severity for reporting and exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsckSeverity {
    /// Should fail the overall fsck run.
    Error,
    /// Acceptable depending on `fsck.<msg-id>` policy.
    Warning,
    /// Informational object-buffer diagnostics (hidden in default human output).
    Info,
}

/// Aggregate fsck outcome.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FsckReport {
    /// Loose and packed objects examined during store validation.
    pub objects_checked: usize,
    /// All recorded findings in discovery order.
    pub issues: Vec<FsckIssue>,
    /// Objects present in the object database but not reachable from [`reachability_roots`].
    pub dangling: Vec<(ObjectKind, ObjectId)>,
}

impl FsckReport {
    /// True when no error-severity issues are present under `options` and `config`.
    #[must_use]
    pub fn ok(&self, config: &ConfigSet, options: &FsckOptions) -> bool {
        !self
            .issues
            .iter()
            .any(|issue| issue_severity(issue, config, options.strict) == FsckSeverity::Error)
    }
}

/// CamelCase Git fsck msg-id for `issue`.
#[must_use]
pub fn issue_msg_id(issue: &FsckIssue) -> &'static str {
    match issue {
        FsckIssue::CorruptLoose { .. } => "corruptObject",
        FsckIssue::HashMismatch { .. } => "hasSha1",
        FsckIssue::BadObject { error, .. } => error.id,
        FsckIssue::PackChecksum { .. } => "badPackChecksum",
        FsckIssue::IdxMismatch { .. } => "badPackIndex",
        FsckIssue::MissingObject { .. } => "missingObject",
        FsckIssue::BrokenLink { .. } => "brokenLink",
        FsckIssue::BadRef(r) => r.msg_id,
        FsckIssue::MidxInvalid { .. } => "badMidx",
        FsckIssue::CommitGraphInvalid { .. } => "badCommitGraph",
    }
}

/// Git default level for object-buffer fsck msg-ids (`Documentation/fsck-msgids.txt` / `fsck.h`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjectFsckDefaultLevel {
    Fatal,
    Error,
    Warn,
    Info,
    Ignore,
}

fn object_fsck_default_level(msg_id: &str) -> ObjectFsckDefaultLevel {
    match msg_id {
        "nulInHeader" | "unterminatedHeader" => ObjectFsckDefaultLevel::Fatal,
        "emptyName" | "fullPathname" | "hasDot" | "hasDotdot" | "hasDotgit" | "nullSha1"
        | "zeroPaddedFilemode" | "nulInCommit" | "largePathname" => ObjectFsckDefaultLevel::Warn,
        "badFilemode"
        | "gitmodulesParse"
        | "gitignoreSymlink"
        | "gitattributesSymlink"
        | "mailmapSymlink"
        | "badTagName"
        | "missingTaggerEntry" => ObjectFsckDefaultLevel::Info,
        "extraHeaderEntry" => ObjectFsckDefaultLevel::Ignore,
        "sha1Collision" => ObjectFsckDefaultLevel::Warn,
        _ => ObjectFsckDefaultLevel::Error,
    }
}

fn severity_from_config(config: &ConfigSet, msg_id: &str) -> Option<FsckSeverity> {
    let key = format!("fsck.{msg_id}");
    match config.get(&key).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("ignore") => Some(FsckSeverity::Warning),
        Some("warn") => Some(FsckSeverity::Warning),
        Some("error") => Some(FsckSeverity::Error),
        _ => None,
    }
}

fn object_buffer_fsck_severity(msg_id: &str, config: &ConfigSet, strict: bool) -> FsckSeverity {
    if let Some(sev) = severity_from_config(config, msg_id) {
        return sev;
    }
    match object_fsck_default_level(msg_id) {
        ObjectFsckDefaultLevel::Fatal | ObjectFsckDefaultLevel::Error => FsckSeverity::Error,
        ObjectFsckDefaultLevel::Warn if strict => FsckSeverity::Error,
        ObjectFsckDefaultLevel::Warn => FsckSeverity::Warning,
        ObjectFsckDefaultLevel::Info | ObjectFsckDefaultLevel::Ignore => FsckSeverity::Info,
    }
}

/// Whether default human fsck output should list this issue (matches Git omitting most INFO by default).
#[must_use]
pub fn issue_visible_by_default(issue: &FsckIssue, config: &ConfigSet, strict: bool) -> bool {
    let severity = issue_severity(issue, config, strict);
    if severity != FsckSeverity::Info {
        return true;
    }
    if let FsckIssue::BadObject { error, .. } = issue {
        return matches!(error.id, "missingTaggerEntry");
    }
    false
}

/// Resolve severity using `fsck.<msg-id>` and `strict`.
#[must_use]
pub fn issue_severity(issue: &FsckIssue, config: &ConfigSet, strict: bool) -> FsckSeverity {
    if let FsckIssue::BadRef(r) = issue {
        return match r.severity {
            crate::refs_fsck::RefsFsckSeverity::Error => FsckSeverity::Error,
            crate::refs_fsck::RefsFsckSeverity::Warning if strict => FsckSeverity::Error,
            crate::refs_fsck::RefsFsckSeverity::Warning => FsckSeverity::Warning,
        };
    }
    let msg_id = issue_msg_id(issue);
    if matches!(issue, FsckIssue::BadObject { .. }) {
        return object_buffer_fsck_severity(msg_id, config, strict);
    }
    if let Some(sev) = severity_from_config(config, msg_id) {
        return sev;
    }
    FsckSeverity::Error
}

/// Format one issue like Git fsck (`level: path: msgId: detail`).
#[must_use]
pub fn format_fsck_line(issue: &FsckIssue, config: &ConfigSet, strict: bool) -> String {
    let level = match issue_severity(issue, config, strict) {
        FsckSeverity::Error => "error",
        FsckSeverity::Warning => "warning",
        FsckSeverity::Info => "info",
    };
    let (path, detail) = issue.line_subject_and_detail();
    format!("{level}: {path}: {}: {detail}", issue_msg_id(issue))
}

impl FsckIssue {
    /// Object the finding is about, when it concerns a single object.
    ///
    /// For [`FsckIssue::BrokenLink`] this is the tree holding the bad entry; for
    /// [`FsckIssue::MissingObject`] it is the missing object itself.
    ///
    /// # Returns
    ///
    /// `None` for pack, ref, multi-pack-index and commit-graph findings.
    #[must_use]
    pub fn oid(&self) -> Option<ObjectId> {
        match self {
            Self::CorruptLoose { oid, .. }
            | Self::HashMismatch { oid, .. }
            | Self::BadObject { oid, .. }
            | Self::MissingObject { oid, .. } => Some(*oid),
            Self::BrokenLink { tree, .. } => Some(*tree),
            Self::PackChecksum { .. }
            | Self::IdxMismatch { .. }
            | Self::BadRef(_)
            | Self::MidxInvalid { .. }
            | Self::CommitGraphInvalid { .. } => None,
        }
    }

    /// File, ref or repository structure the finding was reported against.
    ///
    /// # Returns
    ///
    /// The loose object or `.pack` path, the ref path for [`FsckIssue::BadRef`],
    /// `multi-pack-index` or `commit-graph`; `None` for findings that only name
    /// objects (see [`Self::oid`]).
    #[must_use]
    pub fn location(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::CorruptLoose { path, .. }
            | Self::HashMismatch { path, .. }
            | Self::PackChecksum { pack: path }
            | Self::IdxMismatch { pack: path } => Some(path.to_string_lossy()),
            Self::BadRef(r) => Some(Cow::Borrowed(r.path.as_str())),
            Self::MidxInvalid { .. } => Some(Cow::Borrowed("multi-pack-index")),
            Self::CommitGraphInvalid { .. } => Some(Cow::Borrowed("commit-graph")),
            Self::BadObject { .. } | Self::MissingObject { .. } | Self::BrokenLink { .. } => None,
        }
    }

    /// Self-contained one-line description of the finding.
    ///
    /// Unlike the detail in [`format_fsck_line`], this names everything needed to
    /// read it on its own (for example the tree of a broken link).
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::BadObject { error, .. } if error.id == "sha1Collision" => {
                format!("SHA-1 chosen-prefix collision: {}", error.detail)
            }
            Self::BrokenLink { tree, .. } => {
                format!(
                    "tree {} {}",
                    tree.to_hex(),
                    self.line_subject_and_detail().1
                )
            }
            _ => self.line_subject_and_detail().1,
        }
    }

    /// Subject and detail for a `level: subject: msgId: detail` line, where the
    /// subject already names the object or file the detail is about.
    pub(crate) fn line_subject_and_detail(&self) -> (String, String) {
        match self {
            Self::CorruptLoose { oid, path } => {
                (oid.to_hex(), format!("could not read {}", path.display()))
            }
            Self::HashMismatch { oid, path } => (
                oid.to_hex(),
                format!("loose hash mismatch at {}", path.display()),
            ),
            Self::BadObject { oid, error } => (oid.to_hex(), error.detail.clone()),
            Self::PackChecksum { pack } => (
                pack_file_name(pack),
                format!("pack checksum invalid for {}", pack.display()),
            ),
            Self::IdxMismatch { pack } => (
                pack_file_name(pack),
                format!("index checksum mismatch for {}", pack.display()),
            ),
            Self::MissingObject { oid, referrer } => {
                (oid.to_hex(), missing_object_detail(*oid, *referrer))
            }
            Self::BrokenLink {
                tree,
                name,
                mode,
                target,
            } => (
                tree.to_hex(),
                format!("entry {name} mode {mode:o} -> {}", target.to_hex()),
            ),
            Self::BadRef(r) => (r.path.clone(), r.detail.clone()),
            Self::MidxInvalid { details } => ("multi-pack-index".to_owned(), details.join("; ")),
            Self::CommitGraphInvalid { detail } => ("commit-graph".to_owned(), detail.clone()),
        }
    }
}

fn pack_file_name(pack: &Path) -> String {
    pack.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pack")
        .to_owned()
}

/// Run repository fsck with optional progress and cancellation.
///
/// # Errors
///
/// Propagates object-store, ref, or index I/O failures that abort the run.
/// Returns [`crate::error::Error::UnsupportedObjectStore`] (operation `"fsck"`) when the
/// repository uses a custom primary object store, since fsck verifies the on-disk
/// loose objects and packs.
pub fn fsck(
    repo: &Repository,
    options: &FsckOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<FsckReport> {
    repo.odb().require_files_primary("fsck")?;
    let config = repo.config()?;
    let odb = repo.odb();
    if !options.connectivity_only {
        odb.invalidate_packs();
    }

    let mut issues = Vec::new();
    let mut objects_checked = 0usize;
    let mut store_validated: OidHashSet = oid_hash_set();
    let mut store_links: OidHashMap<ValidatedObjectLinks> = oid_hash_map();

    if !options.connectivity_only {
        let (loose_count, loose_validated, loose_links) =
            verify_loose_objects(repo, &config, options.strict, &mut issues, progress, cancel)?;
        objects_checked += loose_count;
        store_validated.extend(loose_validated);
        store_links.extend(loose_links);
        let (pack_count, pack_validated, pack_links) =
            verify_pack_objects(repo, &config, options.strict, &mut issues, progress, cancel)?;
        objects_checked += pack_count;
        store_validated.extend(pack_validated);
        store_links.extend(pack_links);
    }

    verify_midx_and_commit_graph(repo, &mut issues)?;

    for rissue in refs_fsck(repo, odb, &config, options.strict)? {
        if should_emit(&config, options.strict, rissue.msg_id) {
            issues.push(FsckIssue::BadRef(rissue));
        }
    }

    let mut reachable = oid_hash_set();
    if options.check_connectivity {
        let index_roots_set = index_object_roots(repo)?;
        let index_roots = oid_hash_set_from(&index_roots_set);
        let roots = reachability_roots(repo)?;
        let treats_promisor = crate::promisor::repo_treats_promisor_packs(repo.git_dir(), &config);
        let promisor_parents = if treats_promisor {
            oid_hash_set_from(&crate::promisor::promisor_pack_object_ids(
                &repo.git_dir().join("objects"),
            ))
        } else {
            oid_hash_set()
        };
        let promisor_promised = if treats_promisor {
            Some(oid_hash_set_from(
                &crate::promisor::promisor_expanded_object_ids(repo)?,
            ))
        } else {
            None
        };
        let (walked, connectivity_issues) = walk_connectivity(
            repo,
            &roots,
            &index_roots,
            &store_validated,
            &store_links,
            &config,
            options.strict,
            progress,
            cancel,
            &promisor_parents,
        )?;
        reachable = walked;
        issues.extend(connectivity_issues);

        fsck_index_missing_blobs(
            repo,
            &config,
            options.strict,
            &mut issues,
            promisor_promised.as_ref(),
        )?;
    }
    dedupe_issues(&mut issues);

    let mut dangling = Vec::new();
    if options.check_dangling {
        let mut all_objects: OidHashSet = oid_hash_set();
        odb.for_each_object(&mut |oid| {
            all_objects.insert(*oid);
            ControlFlow::Continue(())
        })?;
        for oid in all_objects {
            if reachable.contains(&oid) {
                continue;
            }
            if let Ok(info) = odb.read_info(&oid) {
                dangling.push((info.kind, oid));
            }
        }
        dangling.sort_by_key(|a| a.1);
    }

    Ok(FsckReport {
        objects_checked,
        issues,
        dangling,
    })
}

fn should_emit(config: &ConfigSet, _strict: bool, msg_id: &str) -> bool {
    let key = format!("fsck.{msg_id}");
    let level = config.get(&key).map(|s| s.to_ascii_lowercase());
    !matches!(level.as_deref(), Some("ignore"))
}

fn push_bad_object(
    issues: &mut Vec<FsckIssue>,
    config: &ConfigSet,
    strict: bool,
    oid: ObjectId,
    err: FsckError,
) {
    if should_emit(config, strict, err.id) {
        issues.push(FsckIssue::BadObject { oid, error: err });
    }
}

/// True when object-buffer fsck failed only with diagnostics that still allow link extraction.
fn object_fsck_allows_link_cache(err: &FsckError, config: &ConfigSet, strict: bool) -> bool {
    matches!(
        object_buffer_fsck_severity(err.id, config, strict),
        FsckSeverity::Warning | FsckSeverity::Info
    )
}

fn push_corrupt_parse(
    issues: &mut Vec<FsckIssue>,
    config: &ConfigSet,
    strict: bool,
    oid: ObjectId,
    err: Error,
) {
    let detail = err.to_string();
    push_bad_object(
        issues,
        config,
        strict,
        oid,
        FsckError::new("corruptObject", detail),
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkRootKind {
    General,
    IndexOnly,
}

/// Connectivity payload extracted while validating the object store (avoids re-inflating packs).
#[derive(Debug, Clone)]
enum ValidatedObjectLinks {
    Commit {
        tree: ObjectId,
        parents: Vec<ObjectId>,
    },
    Tree(Vec<TreeEntry>),
    Tag {
        object: ObjectId,
        object_type: ObjectKind,
    },
    Blob,
}

fn validated_links_for(
    kind: ObjectKind,
    data: &[u8],
    algo: HashAlgo,
) -> Option<ValidatedObjectLinks> {
    match kind {
        ObjectKind::Commit => {
            if let Ok(c) = parse_commit(data) {
                Some(ValidatedObjectLinks::Commit {
                    tree: c.tree,
                    parents: c.parents,
                })
            } else {
                commit_link_targets(data, algo)
                    .map(|(tree, parents)| ValidatedObjectLinks::Commit { tree, parents })
            }
        }
        ObjectKind::Tree => parse_tree(data).ok().map(ValidatedObjectLinks::Tree),
        ObjectKind::Tag => {
            let tag = parse_tag(data).ok()?;
            let object_type = ObjectKind::from_tag_type_field(tag.object_type.as_bytes())?;
            Some(ValidatedObjectLinks::Tag {
                object: tag.object,
                object_type,
            })
        }
        ObjectKind::Blob => Some(ValidatedObjectLinks::Blob),
    }
}

fn record_validated_object(
    validated: &mut OidHashSet,
    links: &mut OidHashMap<ValidatedObjectLinks>,
    oid: ObjectId,
    kind: ObjectKind,
    data: &[u8],
    algo: HashAlgo,
) {
    validated.insert(oid);
    if let Some(parsed) = validated_links_for(kind, data, algo) {
        links.insert(oid, parsed);
    }
}

/// Parse `tree` and `parent` lines from a commit buffer even when author/committer headers fail fsck.
fn commit_link_targets(data: &[u8], algo: HashAlgo) -> Option<(ObjectId, Vec<ObjectId>)> {
    let hex_len = algo.hex_len();
    if data.len() < 5 + hex_len || !data.starts_with(b"tree ") {
        return None;
    }
    let mut i = 5usize;
    let tree_hex = std::str::from_utf8(&data[i..i + hex_len]).ok()?;
    let tree = ObjectId::from_hex(tree_hex).ok()?;
    i += hex_len;
    if data.get(i) == Some(&b'\n') {
        i += 1;
    }
    let mut parents = Vec::new();
    while data
        .get(i..)
        .is_some_and(|tail| tail.starts_with(b"parent "))
    {
        i += 7;
        let parent_hex = std::str::from_utf8(&data[i..i + hex_len]).ok()?;
        parents.push(ObjectId::from_hex(parent_hex).ok()?);
        i += hex_len;
        if data.get(i) == Some(&b'\n') {
            i += 1;
        }
    }
    Some((tree, parents))
}

fn dedupe_issues(issues: &mut Vec<FsckIssue>) {
    let mut corrupt_or_bad = oid_hash_set();
    for issue in issues.iter() {
        match issue {
            FsckIssue::CorruptLoose { oid, .. } | FsckIssue::HashMismatch { oid, .. } => {
                corrupt_or_bad.insert(*oid);
            }
            FsckIssue::BadObject { oid, error } if error.id == "corruptObject" => {
                corrupt_or_bad.insert(*oid);
            }
            _ => {}
        }
    }
    let mut seen = HashSet::<String>::new();
    issues.retain(|issue| {
        if let FsckIssue::MissingObject { oid, .. } = issue {
            if corrupt_or_bad.contains(oid) {
                return false;
            }
        }
        let key = match issue {
            FsckIssue::BadObject { oid, error } => format!("bad:{}:{}", oid, error.id),
            FsckIssue::MissingObject { oid, referrer } => {
                format!("missing:{}:{referrer:?}", oid)
            }
            FsckIssue::BrokenLink {
                tree, name, target, ..
            } => format!("link:{}:{}:{}", tree, name, target),
            FsckIssue::BadRef(r) => format!("ref:{}:{}", r.path, r.msg_id),
            FsckIssue::CorruptLoose { oid, .. } => format!("corrupt:{}", oid),
            FsckIssue::HashMismatch { oid, .. } => format!("hash:{}", oid),
            FsckIssue::PackChecksum { pack } => format!("packsum:{}", pack.display()),
            FsckIssue::IdxMismatch { pack } => format!("idx:{}", pack.display()),
            FsckIssue::MidxInvalid { .. } => "midx".to_owned(),
            FsckIssue::CommitGraphInvalid { .. } => "commit-graph".to_owned(),
        };
        seen.insert(key)
    });
}

enum LooseCheckOutcome {
    Clean,
    Issue(FsckIssue),
}

fn verify_loose_objects(
    repo: &Repository,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<(usize, OidHashSet, OidHashMap<ValidatedObjectLinks>)> {
    let odb = repo.odb();
    let algo = odb.hash_algo();
    let loose = odb.enumerate_local_loose_objects()?;
    let total = loose.len();
    progress.start(ProgressPhase::IndexingObjects, Some(total as u64));
    let par_opts: ParallelOptions = repo.thread_policy().resolve(ParallelOp::IndexPack, config);
    let fsck_opts = FsckObjectOptions::new(odb.hash_algo());
    let mut dot = DotFsckTracker::default();

    if cancel.is_cancelled() {
        return Err(Error::Message("cancelled".into()));
    }
    let results = match try_par_map::<_, _, String, _>(&loose, par_opts, total, |(oid, path)| {
        let read = Odb::read_loose_verify_oid(path, oid);
        match read {
            Err(Error::LooseHashMismatch { .. }) => {
                Ok(LooseCheckOutcome::Issue(FsckIssue::HashMismatch {
                    oid: *oid,
                    path: path.clone(),
                }))
            }
            Err(Error::HashCollision { oid: colliding }) => Ok(LooseCheckOutcome::Issue(
                FsckIssue::BadObject {
                    oid: *oid,
                    error: FsckError::new(
                        "sha1Collision",
                        format!(
                            "SHA-1 collision detected while verifying loose object (digest {colliding})"
                        ),
                    ),
                },
            )),
            Err(_) => Ok(LooseCheckOutcome::Issue(FsckIssue::CorruptLoose {
                oid: *oid,
                path: path.clone(),
            })),
            Ok(obj) => {
                if let Err(err) = fsck_object(obj.kind, &obj.data, fsck_opts) {
                    return Ok(LooseCheckOutcome::Issue(FsckIssue::BadObject {
                        oid: *oid,
                        error: err,
                    }));
                }
                Ok(LooseCheckOutcome::Clean)
            }
        }
    }) {
        Ok(v) => v,
        Err(e) => return Err(Error::Message(format!("parallel fsck: {e}"))),
    };

    let mut checked = 0usize;
    let mut validated = oid_hash_set();
    let mut links = oid_hash_map();
    for (item, outcome) in loose.iter().zip(results) {
        checked += 1;
        if checked.is_multiple_of(128) {
            progress.set(checked as u64);
            if cancel.is_cancelled() {
                return Err(Error::Message("cancelled".into()));
            }
        }
        match outcome {
            LooseCheckOutcome::Clean => {
                if let Ok(obj) = Odb::read_loose_verify_oid(&item.1, &item.0) {
                    record_validated_object(
                        &mut validated,
                        &mut links,
                        item.0,
                        obj.kind,
                        &obj.data,
                        algo,
                    );
                    apply_dot_checks(
                        &mut dot, issues, config, strict, &item.0, obj.kind, &obj.data,
                    );
                }
            }
            LooseCheckOutcome::Issue(issue) => match &issue {
                FsckIssue::BadObject { oid, error } => {
                    push_bad_object(issues, config, strict, *oid, error.clone());
                    if object_fsck_allows_link_cache(error, config, strict) {
                        if let Ok(obj) = Odb::read_loose_verify_oid(&item.1, &item.0) {
                            record_validated_object(
                                &mut validated,
                                &mut links,
                                item.0,
                                obj.kind,
                                &obj.data,
                                algo,
                            );
                            apply_dot_checks(
                                &mut dot, issues, config, strict, &item.0, obj.kind, &obj.data,
                            );
                        }
                    }
                }
                other => {
                    if should_emit(config, strict, issue_msg_id(other)) {
                        issues.push(other.clone());
                    }
                }
            },
        }
    }
    for pending in dot.finish_pending(odb)? {
        push_dot_issue(issues, config, strict, pending);
    }
    progress.finish();
    Ok((checked, validated, links))
}

fn pack_object_read_issue(oid: ObjectId, pack: &Path, err: Error) -> FsckIssue {
    match err {
        Error::CorruptObject(msg) if msg.contains("checksum") && msg.contains("index") => {
            FsckIssue::IdxMismatch {
                pack: pack.to_path_buf(),
            }
        }
        Error::CorruptObject(msg)
            if msg.contains("trailing checksum")
                || msg.contains("pack trailing checksum")
                || msg.contains("pack checksum") =>
        {
            FsckIssue::PackChecksum {
                pack: pack.to_path_buf(),
            }
        }
        Error::ObjectNotFound(_) => FsckIssue::MissingObject {
            oid,
            referrer: MissingObjectReferrer::Pack,
        },
        Error::HashCollision { oid: colliding } => FsckIssue::BadObject {
            oid,
            error: FsckError::new(
                "sha1Collision",
                format!(
                    "SHA-1 collision detected while verifying pack object (digest {colliding})"
                ),
            ),
        },
        Error::CorruptObject(_) | Error::Zlib(_) => FsckIssue::BadObject {
            oid,
            error: FsckError::new("corruptObject", err.to_string()),
        },
        other => FsckIssue::BadObject {
            oid,
            error: FsckError::new("corruptObject", other.to_string()),
        },
    }
}

fn apply_dot_checks(
    dot: &mut DotFsckTracker,
    issues: &mut Vec<FsckIssue>,
    config: &ConfigSet,
    strict: bool,
    oid: &ObjectId,
    kind: ObjectKind,
    data: &[u8],
) {
    if kind == ObjectKind::Tree {
        if let Ok(tree_issues) = dot.on_tree(oid, data) {
            for dot_issue in tree_issues {
                push_dot_issue(issues, config, strict, dot_issue);
            }
        }
    }
    for dot_issue in dot.on_object(oid, kind, data) {
        push_dot_issue(issues, config, strict, dot_issue);
    }
}

fn push_dot_issue(
    issues: &mut Vec<FsckIssue>,
    config: &ConfigSet,
    strict: bool,
    dot: DotFsckIssue,
) {
    let (oid, id, detail) = match dot {
        DotFsckIssue::TreeSymlink {
            tree_oid,
            id,
            detail,
        } => (tree_oid, id, detail.to_owned()),
        DotFsckIssue::NonBlobDotFile {
            oid, id, detail, ..
        } => (oid, id, detail.to_owned()),
        DotFsckIssue::BlobGitmodules {
            blob_oid,
            id,
            detail,
        } => (blob_oid, id, detail),
        DotFsckIssue::BlobGitattributes {
            blob_oid,
            id,
            detail,
        } => (blob_oid, id, detail.to_owned()),
    };
    if should_emit(config, strict, id) {
        issues.push(FsckIssue::BadObject {
            oid,
            error: FsckError::new(id, detail),
        });
    }
}

fn verify_pack_objects(
    repo: &Repository,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<(usize, OidHashSet, OidHashMap<ValidatedObjectLinks>)> {
    let odb = repo.odb();
    let algo = odb.hash_algo();
    let fsck_opts = FsckObjectOptions::new(algo);
    let Some(objects_dir) = odb.files_objects_dir() else {
        return Ok((0, oid_hash_set(), oid_hash_map()));
    };
    let pack_dir = objects_dir.join("pack");
    if !pack_dir.is_dir() {
        return Ok((0, oid_hash_set(), oid_hash_map()));
    }
    let packs: Vec<PathBuf> = fs::read_dir(&pack_dir)
        .map_err(Error::Io)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "idx"))
        .collect();
    progress.start(ProgressPhase::IndexingObjects, Some(packs.len() as u64));
    let par_opts: ParallelOptions = repo.thread_policy().resolve(ParallelOp::IndexPack, config);
    let mut checked = 0usize;
    let mut validated = oid_hash_set();
    let mut links = oid_hash_map();
    let mut dot = DotFsckTracker::default();
    for (i, idx_path) in packs.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(Error::Message("cancelled".into()));
        }
        let pack_path = idx_path.with_extension("pack");
        let (prep, trailer_issue) = match prepare_pack_for_object_fsck(idx_path) {
            Ok(v) => v,
            Err(Error::CorruptObject(msg)) => {
                let issue = if msg.contains("checksum") && msg.contains("index") {
                    FsckIssue::IdxMismatch { pack: pack_path }
                } else {
                    FsckIssue::PackChecksum { pack: pack_path }
                };
                issues.push(issue);
                progress.set((i + 1) as u64);
                continue;
            }
            Err(e) => return Err(e),
        };
        if let Some(trailer) = trailer_issue {
            let issue = match trailer {
                PackFsckTrailerIssue::PackChecksum => FsckIssue::PackChecksum {
                    pack: pack_path.clone(),
                },
                PackFsckTrailerIssue::IdxMismatch => FsckIssue::IdxMismatch {
                    pack: pack_path.clone(),
                },
            };
            if should_emit(config, strict, issue_msg_id(&issue)) {
                issues.push(issue);
            }
        }
        let object_count = prep.records.len();
        checked += object_count;
        validated.reserve(object_count);
        links.reserve(object_count);
        if cancel.is_cancelled() {
            return Err(Error::Message("cancelled".into()));
        }
        let outcomes = verify_pack_objects_offset_order(prep, odb, par_opts)?;
        for (n, outcome) in outcomes.into_iter().enumerate() {
            if n.is_multiple_of(128) && cancel.is_cancelled() {
                return Err(Error::Message("cancelled".into()));
            }
            match outcome {
                PackFsckObjectOutcome::Failed { oid, err } => {
                    let issue = pack_object_read_issue(oid, &pack_path, err);
                    if should_emit(config, strict, issue_msg_id(&issue)) {
                        issues.push(issue);
                    }
                }
                PackFsckObjectOutcome::Clean(item) => {
                    let oid = item.oid;
                    let kind = item.kind;
                    let data = item.data;
                    if let Err(err) = fsck_object(kind, &data, fsck_opts) {
                        push_bad_object(issues, config, strict, oid, err.clone());
                        if object_fsck_allows_link_cache(&err, config, strict) {
                            record_validated_object(
                                &mut validated,
                                &mut links,
                                oid,
                                kind,
                                &data,
                                algo,
                            );
                            apply_dot_checks(&mut dot, issues, config, strict, &oid, kind, &data);
                        }
                        continue;
                    }
                    record_validated_object(&mut validated, &mut links, oid, kind, &data, algo);
                    apply_dot_checks(&mut dot, issues, config, strict, &oid, kind, &data);
                }
            }
        }
        progress.set((i + 1) as u64);
    }
    for pending in dot.finish_pending(odb)? {
        push_dot_issue(issues, config, strict, pending);
    }
    progress.finish();
    Ok((checked, validated, links))
}

fn verify_midx_and_commit_graph(repo: &Repository, issues: &mut Vec<FsckIssue>) -> Result<()> {
    let odb = repo.odb();
    if let Some(objects_dir) = odb.files_objects_dir() {
        let pack_dir = objects_dir.join("pack");
        if pack_dir.join("multi-pack-index").is_file() {
            if let Err(errs) = verify_midx(&pack_dir) {
                issues.push(FsckIssue::MidxInvalid { details: errs });
            }
        }
        match CommitGraphChain::try_load(objects_dir) {
            Ok(None) => {}
            Ok(Some(_)) => {
                if let Err(e) = CommitGraphChain::verify_on_disk(objects_dir) {
                    issues.push(FsckIssue::CommitGraphInvalid {
                        detail: e.to_string(),
                    });
                }
            }
            Err(e) => issues.push(FsckIssue::CommitGraphInvalid {
                detail: e.to_string(),
            }),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_connectivity(
    repo: &Repository,
    roots: &[ObjectId],
    index_roots: &OidHashSet,
    store_validated: &OidHashSet,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    config: &ConfigSet,
    strict: bool,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    promisor_parents: &OidHashSet,
) -> Result<(OidHashSet, Vec<FsckIssue>)> {
    let odb = repo.odb();
    let algo = odb.hash_algo();
    let fsck_opts = FsckObjectOptions::new(algo);
    let mut issues = Vec::new();
    let mut dot = DotFsckTracker::default();
    let mut seen: OidHashSet = oid_hash_set();
    let mut queue: VecDeque<(ObjectId, ObjectId, WalkRootKind)> = VecDeque::new();
    for &root in roots {
        if seen.insert(root) {
            let kind = if index_roots.contains(&root) {
                WalkRootKind::IndexOnly
            } else {
                WalkRootKind::General
            };
            queue.push_back((root, ObjectId::zero(), kind));
        }
    }
    progress.start(ProgressPhase::CopyingObjects, None);
    let mut visited = 0u64;
    while let Some((oid, parent, root_kind)) = queue.pop_front() {
        if cancel.is_cancelled() {
            return Err(Error::Message("cancelled".into()));
        }
        visited += 1;
        if visited.is_multiple_of(256) {
            progress.set(visited);
        }
        if let Some(links) = store_links.get(&oid) {
            walk_from_validated_links(
                odb,
                store_links,
                config,
                strict,
                &mut issues,
                &mut queue,
                &mut seen,
                oid,
                parent,
                root_kind,
                links,
                promisor_parents,
            );
            continue;
        }
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) => {
                if root_kind == WalkRootKind::IndexOnly && parent.is_zero() {
                    // Report once from [`fsck_index_missing_blobs`].
                } else if should_emit(config, strict, "missingObject") {
                    issues.push(FsckIssue::MissingObject {
                        oid,
                        referrer: MissingObjectReferrer::Object(parent),
                    });
                }
                continue;
            }
        };
        if !store_validated.contains(&oid) {
            if let Err(err) = fsck_object(obj.kind, &obj.data, fsck_opts) {
                push_bad_object(&mut issues, config, strict, oid, err);
            }
            apply_dot_checks(
                &mut dot,
                &mut issues,
                config,
                strict,
                &oid,
                obj.kind,
                &obj.data,
            );
        }

        match obj.kind {
            ObjectKind::Commit => {
                let Some((tree, parents)) = (match parse_commit(&obj.data) {
                    Ok(c) => Some((c.tree, c.parents)),
                    Err(_) => commit_link_targets(&obj.data, algo),
                }) else {
                    continue;
                };
                for p in parents {
                    follow_typed_link(
                        odb,
                        store_links,
                        config,
                        strict,
                        &mut issues,
                        &mut queue,
                        &mut seen,
                        p,
                        oid,
                        ObjectKind::Commit,
                        "parent",
                        promisor_parents,
                    );
                }
                follow_typed_link(
                    odb,
                    store_links,
                    config,
                    strict,
                    &mut issues,
                    &mut queue,
                    &mut seen,
                    tree,
                    oid,
                    ObjectKind::Tree,
                    "tree",
                    promisor_parents,
                );
            }
            ObjectKind::Tree => {
                walk_tree_entries_from_data(
                    odb,
                    store_links,
                    config,
                    strict,
                    &mut issues,
                    &mut queue,
                    &mut seen,
                    oid,
                    &obj.data,
                    algo.len(),
                    promisor_parents,
                );
            }
            ObjectKind::Tag => {
                let tag = match parse_tag(&obj.data) {
                    Ok(t) => t,
                    Err(e) => {
                        push_corrupt_parse(&mut issues, config, strict, oid, e);
                        continue;
                    }
                };
                let Some(expected) = ObjectKind::from_tag_type_field(tag.object_type.as_bytes())
                else {
                    continue;
                };
                follow_typed_link(
                    odb,
                    store_links,
                    config,
                    strict,
                    &mut issues,
                    &mut queue,
                    &mut seen,
                    tag.object,
                    oid,
                    expected,
                    "object",
                    promisor_parents,
                );
            }
            ObjectKind::Blob => {}
        }
    }
    for pending in dot.finish_pending(odb)? {
        push_dot_issue(&mut issues, config, strict, pending);
    }
    progress.finish();
    Ok((seen, issues))
}

fn enqueue(
    queue: &mut VecDeque<(ObjectId, ObjectId, WalkRootKind)>,
    seen: &mut OidHashSet,
    oid: ObjectId,
    parent: ObjectId,
) {
    if seen.insert(oid) {
        queue.push_back((oid, parent, WalkRootKind::General));
    }
}

fn link_kind_matches(links: &ValidatedObjectLinks, expected: ObjectKind) -> bool {
    matches!(
        (links, expected),
        (ValidatedObjectLinks::Commit { .. }, ObjectKind::Commit)
            | (ValidatedObjectLinks::Tree(_), ObjectKind::Tree)
            | (ValidatedObjectLinks::Tag { .. }, ObjectKind::Tag)
            | (ValidatedObjectLinks::Blob, ObjectKind::Blob)
    )
}

enum ChildKindProbe {
    Matches,
    WrongKind,
    Missing,
}

fn child_exists_with_kind(
    odb: &Odb,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    target: ObjectId,
    expected: ObjectKind,
) -> ChildKindProbe {
    if let Some(links) = store_links.get(&target) {
        return if link_kind_matches(links, expected) {
            ChildKindProbe::Matches
        } else {
            ChildKindProbe::WrongKind
        };
    }
    match odb.read(&target) {
        Ok(child) if child.kind == expected => ChildKindProbe::Matches,
        Ok(_) => ChildKindProbe::WrongKind,
        Err(_) => ChildKindProbe::Missing,
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_tree_entries_from_data(
    odb: &Odb,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    queue: &mut VecDeque<(ObjectId, ObjectId, WalkRootKind)>,
    seen: &mut OidHashSet,
    tree_oid: ObjectId,
    tree_data: &[u8],
    oid_len: usize,
    promisor_parents: &OidHashSet,
) {
    let entries = TreeRefIter::with_oid_len(tree_data, oid_len).collect::<Result<Vec<_>>>();
    let entries = match entries {
        Ok(entries) => entries,
        Err(err) => {
            push_corrupt_parse(issues, config, strict, tree_oid, err);
            return;
        }
    };
    for entry in entries {
        walk_one_tree_entry(
            odb,
            store_links,
            config,
            strict,
            issues,
            queue,
            seen,
            tree_oid,
            entry,
            promisor_parents,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_tree_entries(
    odb: &Odb,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    queue: &mut VecDeque<(ObjectId, ObjectId, WalkRootKind)>,
    seen: &mut OidHashSet,
    tree_oid: ObjectId,
    entries: &[TreeEntry],
    promisor_parents: &OidHashSet,
) {
    for entry in entries {
        walk_one_tree_entry(
            odb,
            store_links,
            config,
            strict,
            issues,
            queue,
            seen,
            tree_oid,
            TreeEntryRef {
                mode: entry.mode,
                name: &entry.name,
                oid: entry.oid,
            },
            promisor_parents,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_one_tree_entry(
    odb: &Odb,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    queue: &mut VecDeque<(ObjectId, ObjectId, WalkRootKind)>,
    seen: &mut OidHashSet,
    tree_oid: ObjectId,
    entry: TreeEntryRef<'_>,
    promisor_parents: &OidHashSet,
) {
    if entry.mode == MODE_GITLINK {
        return;
    }
    if let Some(expected) = kind_for_tree_mode(entry.mode) {
        match child_exists_with_kind(odb, store_links, entry.oid, expected) {
            ChildKindProbe::Missing => {
                if expected == ObjectKind::Blob && promisor_parents.contains(&tree_oid) {
                    return;
                }
                if should_emit(config, strict, "missingObject") {
                    issues.push(FsckIssue::MissingObject {
                        oid: entry.oid,
                        referrer: MissingObjectReferrer::Object(tree_oid),
                    });
                }
            }
            ChildKindProbe::WrongKind => {
                if should_emit(config, strict, "brokenLink") {
                    issues.push(FsckIssue::BrokenLink {
                        tree: tree_oid,
                        name: String::from_utf8_lossy(entry.name).into_owned(),
                        mode: entry.mode,
                        target: entry.oid,
                    });
                }
            }
            ChildKindProbe::Matches => enqueue(queue, seen, entry.oid, tree_oid),
        }
    } else if should_emit(config, strict, "brokenLink") {
        issues.push(FsckIssue::BrokenLink {
            tree: tree_oid,
            name: String::from_utf8_lossy(entry.name).into_owned(),
            mode: entry.mode,
            target: entry.oid,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_from_validated_links(
    odb: &Odb,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    queue: &mut VecDeque<(ObjectId, ObjectId, WalkRootKind)>,
    seen: &mut OidHashSet,
    oid: ObjectId,
    _parent: ObjectId,
    _root_kind: WalkRootKind,
    links: &ValidatedObjectLinks,
    promisor_parents: &OidHashSet,
) {
    match links {
        ValidatedObjectLinks::Commit { tree, parents } => {
            for p in parents {
                follow_typed_link(
                    odb,
                    store_links,
                    config,
                    strict,
                    issues,
                    queue,
                    seen,
                    *p,
                    oid,
                    ObjectKind::Commit,
                    "parent",
                    promisor_parents,
                );
            }
            follow_typed_link(
                odb,
                store_links,
                config,
                strict,
                issues,
                queue,
                seen,
                *tree,
                oid,
                ObjectKind::Tree,
                "tree",
                promisor_parents,
            );
        }
        ValidatedObjectLinks::Tree(entries) => {
            walk_tree_entries(
                odb,
                store_links,
                config,
                strict,
                issues,
                queue,
                seen,
                oid,
                entries,
                promisor_parents,
            );
        }
        ValidatedObjectLinks::Tag {
            object,
            object_type,
        } => {
            follow_typed_link(
                odb,
                store_links,
                config,
                strict,
                issues,
                queue,
                seen,
                *object,
                oid,
                *object_type,
                "object",
                promisor_parents,
            );
        }
        ValidatedObjectLinks::Blob => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn follow_typed_link(
    odb: &Odb,
    store_links: &OidHashMap<ValidatedObjectLinks>,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    queue: &mut VecDeque<(ObjectId, ObjectId, WalkRootKind)>,
    seen: &mut OidHashSet,
    target: ObjectId,
    parent: ObjectId,
    expected: ObjectKind,
    link_name: &str,
    promisor_parents: &OidHashSet,
) {
    match child_exists_with_kind(odb, store_links, target, expected) {
        ChildKindProbe::Missing => {
            if expected == ObjectKind::Blob && promisor_parents.contains(&parent) {
                return;
            }
            if should_emit(config, strict, "missingObject") {
                issues.push(FsckIssue::MissingObject {
                    oid: target,
                    referrer: MissingObjectReferrer::Object(parent),
                });
            }
        }
        ChildKindProbe::WrongKind => {
            if should_emit(config, strict, "brokenLink") {
                issues.push(FsckIssue::BrokenLink {
                    tree: parent,
                    name: link_name.to_owned(),
                    mode: 0,
                    target,
                });
            }
        }
        ChildKindProbe::Matches => enqueue(queue, seen, target, parent),
    }
}

fn kind_for_tree_mode(mode: u32) -> Option<ObjectKind> {
    match mode {
        0o040000 => Some(ObjectKind::Tree),
        // Legacy non-executable file mode; Git still follows the link while reporting badFilemode.
        0o100644 | 0o100664 | 0o100755 | 0o120000 => Some(ObjectKind::Blob),
        0o160000 => Some(ObjectKind::Commit),
        _ => None,
    }
}

fn issue_oids(issues: &[FsckIssue]) -> OidHashSet {
    let mut oids = oid_hash_set();
    for issue in issues {
        if let Some(oid) = match issue {
            FsckIssue::CorruptLoose { oid, .. }
            | FsckIssue::HashMismatch { oid, .. }
            | FsckIssue::BadObject { oid, .. }
            | FsckIssue::MissingObject { oid, .. } => Some(*oid),
            FsckIssue::BrokenLink { tree, .. } => Some(*tree),
            _ => None,
        } {
            oids.insert(oid);
        }
    }
    oids
}

fn fsck_index_missing_blobs(
    repo: &Repository,
    config: &ConfigSet,
    strict: bool,
    issues: &mut Vec<FsckIssue>,
    promisor_promised: Option<&OidHashSet>,
) -> Result<()> {
    let index = repo.load_index()?;
    let odb = repo.odb();
    let mut already_reported = issue_oids(issues);
    for entry in index.entries() {
        if entry.mode == MODE_GITLINK || entry.oid.is_zero() {
            continue;
        }
        if already_reported.contains(&entry.oid) {
            continue;
        }
        if odb.read(&entry.oid).is_err() {
            if promisor_promised.is_some_and(|promised| promised.contains(&entry.oid)) {
                continue;
            }
            if should_emit(config, strict, "missingObject") {
                already_reported.insert(entry.oid);
                issues.push(FsckIssue::MissingObject {
                    oid: entry.oid,
                    referrer: MissingObjectReferrer::Index,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::repo::InitOptions;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn missing_object_detail_distinguishes_index_and_pack() {
        let oid = ObjectId::from_hex("1212121212121212121212121212121212121212").unwrap();
        assert!(missing_object_detail(oid, MissingObjectReferrer::Index).contains("index"));
        assert!(missing_object_detail(oid, MissingObjectReferrer::Pack).contains("pack"));
    }

    #[test]
    fn index_missing_object_detail_omits_zero_referrer() {
        let oid = ObjectId::from_hex("abcd1234abcd1234abcd1234abcd1234abcd1234").unwrap();
        let issue = FsckIssue::MissingObject {
            oid,
            referrer: MissingObjectReferrer::Index,
        };
        assert!(issue.detail().contains("staged in index"));

        let pack = FsckIssue::MissingObject {
            oid,
            referrer: MissingObjectReferrer::Pack,
        };
        assert!(pack.detail().contains("missing from pack"));
    }

    #[test]
    fn bad_ref_line_matches_refs_verify_line() {
        let refs = RefsFsckIssue {
            severity: crate::refs_fsck::RefsFsckSeverity::Error,
            path: "refs/heads/main".to_owned(),
            msg_id: "badSymref",
            detail: "symref target missing".to_owned(),
        };
        let line = format_fsck_line(&FsckIssue::BadRef(refs.clone()), &ConfigSet::new(), false);
        assert!(line.contains("badSymref"));
        assert_eq!(line, crate::refs_fsck::format_refs_fsck_line(&refs));
    }

    #[test]
    fn issue_oid_location_and_detail_per_variant() {
        let oid = ObjectId::from_hex("1212121212121212121212121212121212121212").unwrap();
        let other = ObjectId::from_hex("3434343434343434343434343434343434343434").unwrap();
        let pack = PathBuf::from("/repo/objects/pack/pack-1.pack");
        let cases: Vec<(FsckIssue, Option<ObjectId>, Option<&str>, String)> = vec![
            (
                FsckIssue::CorruptLoose {
                    oid,
                    path: PathBuf::from("/repo/objects/12/12"),
                },
                Some(oid),
                Some("/repo/objects/12/12"),
                "could not read /repo/objects/12/12".to_owned(),
            ),
            (
                FsckIssue::HashMismatch {
                    oid,
                    path: PathBuf::from("/repo/objects/12/12"),
                },
                Some(oid),
                Some("/repo/objects/12/12"),
                "loose hash mismatch at /repo/objects/12/12".to_owned(),
            ),
            (
                FsckIssue::BadObject {
                    oid,
                    error: FsckError::new("badTree", "malformed tree"),
                },
                Some(oid),
                None,
                "malformed tree".to_owned(),
            ),
            (
                FsckIssue::BadObject {
                    oid,
                    error: FsckError::new("sha1Collision", "digest x"),
                },
                Some(oid),
                None,
                "SHA-1 chosen-prefix collision: digest x".to_owned(),
            ),
            (
                FsckIssue::PackChecksum { pack: pack.clone() },
                None,
                Some("/repo/objects/pack/pack-1.pack"),
                "pack checksum invalid for /repo/objects/pack/pack-1.pack".to_owned(),
            ),
            (
                FsckIssue::IdxMismatch { pack: pack.clone() },
                None,
                Some("/repo/objects/pack/pack-1.pack"),
                "index checksum mismatch for /repo/objects/pack/pack-1.pack".to_owned(),
            ),
            (
                FsckIssue::MissingObject {
                    oid: other,
                    referrer: MissingObjectReferrer::Object(oid),
                },
                Some(other),
                None,
                missing_object_detail(other, MissingObjectReferrer::Object(oid)),
            ),
            (
                FsckIssue::BrokenLink {
                    tree: oid,
                    name: "a.txt".to_owned(),
                    mode: 0o100644,
                    target: other,
                },
                Some(oid),
                None,
                format!(
                    "tree {} entry a.txt mode 100644 -> {}",
                    oid.to_hex(),
                    other.to_hex()
                ),
            ),
            (
                FsckIssue::BadRef(crate::refs_fsck::RefsFsckIssue {
                    severity: crate::refs_fsck::RefsFsckSeverity::Warning,
                    path: "refs/heads/x".to_owned(),
                    msg_id: "badRefName",
                    detail: "bad name".to_owned(),
                }),
                None,
                Some("refs/heads/x"),
                "bad name".to_owned(),
            ),
            (
                FsckIssue::MidxInvalid {
                    details: vec!["a".to_owned(), "b".to_owned()],
                },
                None,
                Some("multi-pack-index"),
                "a; b".to_owned(),
            ),
            (
                FsckIssue::CommitGraphInvalid {
                    detail: "truncated".to_owned(),
                },
                None,
                Some("commit-graph"),
                "truncated".to_owned(),
            ),
        ];
        for (issue, want_oid, want_location, want_detail) in cases {
            assert_eq!(issue.oid(), want_oid, "{issue:?}");
            assert_eq!(issue.location().as_deref(), want_location, "{issue:?}");
            assert_eq!(issue.detail(), want_detail, "{issue:?}");
        }
    }

    #[test]
    fn fsck_line_subject_does_not_repeat_in_detail() {
        let config = ConfigSet::default();
        let oid = ObjectId::from_hex("1212121212121212121212121212121212121212").unwrap();
        let other = ObjectId::from_hex("3434343434343434343434343434343434343434").unwrap();
        let broken = FsckIssue::BrokenLink {
            tree: oid,
            name: "a.txt".to_owned(),
            mode: 0o100644,
            target: other,
        };
        assert_eq!(
            format_fsck_line(&broken, &config, false),
            format!(
                "error: {}: brokenLink: entry a.txt mode 100644 -> {}",
                oid.to_hex(),
                other.to_hex()
            )
        );
        let pack = FsckIssue::PackChecksum {
            pack: PathBuf::from("/repo/objects/pack/pack-1.pack"),
        };
        assert_eq!(
            format_fsck_line(&pack, &config, false),
            "error: pack-1.pack: badPackChecksum: pack checksum invalid for /repo/objects/pack/pack-1.pack"
        );
        let collision = FsckIssue::BadObject {
            oid,
            error: FsckError::new("sha1Collision", "digest x"),
        };
        assert_eq!(
            format_fsck_line(&collision, &config, false),
            format!("warning: {}: sha1Collision: digest x", oid.to_hex())
        );
    }

    #[test]
    fn dedupe_drops_missing_when_corrupt_reported() {
        let oid = ObjectId::from_hex("1212121212121212121212121212121212121212").unwrap();
        let mut issues = vec![
            FsckIssue::CorruptLoose {
                oid,
                path: PathBuf::from("/tmp/obj"),
            },
            FsckIssue::MissingObject {
                oid,
                referrer: MissingObjectReferrer::Object(
                    ObjectId::from_hex("abababababababababababababababababababab").unwrap(),
                ),
            },
        ];
        dedupe_issues(&mut issues);
        assert_eq!(issues.len(), 1);
        assert!(matches!(issues[0], FsckIssue::CorruptLoose { .. }));
    }

    #[test]
    fn clean_repo_fsck_ok() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("f"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "c".into(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
        )
        .expect("commit");
        let config = repo.config().expect("config");
        let report = fsck(
            &repo,
            &FsckOptions::full_repository(true),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("fsck");
        assert!(report.ok(&config, &FsckOptions::full_repository(true)));
        assert!(report.issues.is_empty());
    }

    #[test]
    fn corrupt_tree_with_valid_prefix_does_not_enqueue_children() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let missing_child = ObjectId::from_hex("abababababababababababababababababababab").unwrap();
        let mut tree_bytes = Vec::new();
        tree_bytes.extend_from_slice(b"100644 a\0");
        tree_bytes.extend_from_slice(missing_child.as_bytes());
        tree_bytes.extend_from_slice(b"\xffcorrupt suffix");
        let tree_oid = repo
            .odb()
            .write(ObjectKind::Tree, &tree_bytes)
            .expect("tree");
        let commit_body = format!(
            "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
            tree_oid.to_hex()
        );
        let commit_oid = repo
            .odb()
            .write(ObjectKind::Commit, commit_body.as_bytes())
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &commit_oid).expect("ref");

        let report = fsck(
            &repo,
            &FsckOptions::full_repository(false),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("fsck");
        assert!(
            report.issues.iter().any(|issue| {
                matches!(
                    issue,
                    FsckIssue::BadObject { oid, error }
                        if *oid == tree_oid && error.id == "corruptObject"
                )
            }),
            "expected corrupt tree report: {:?}",
            report.issues
        );
        assert!(
            !report.issues.iter().any(|issue| {
                matches!(
                    issue,
                    FsckIssue::MissingObject { oid, .. } if *oid == missing_child
                )
            }),
            "must not walk prefix entries before parse failure: {:?}",
            report.issues
        );
    }

    #[test]
    fn commit_tree_pointing_at_blob_is_broken_link() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, b"not-a-tree")
            .expect("blob");
        let commit_body = format!(
            "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
            blob.to_hex()
        );
        let bad_commit = repo
            .odb()
            .write(ObjectKind::Commit, commit_body.as_bytes())
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &bad_commit).expect("update ref");

        let report = fsck(
            &repo,
            &FsckOptions::full_repository(false),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("fsck");
        assert!(
            report.issues.iter().any(|issue| {
                matches!(
                    issue,
                    FsckIssue::BrokenLink { target, .. } if *target == blob
                )
            }),
            "commit tree must not accept a blob oid: {:?}",
            report.issues
        );
    }
}
