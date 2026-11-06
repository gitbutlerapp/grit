//! Resolve remotes and refspecs for [`Remote::push`](crate::remote::Remote::push).
//!
//! [`resolve_push`] turns a [`PushRequest`] (CLI flags and positional refspecs)
//! into a [`ResolvedPush`] of concrete [`PushRefSpec`] entries, matching Git's
//! push refspec and remote-selection rules.
//!
//! A typical push classifies its positionals with [`resolve_push_invocation`], loads the remote's
//! advertisement with [`load_push_plan_context`], plans with [`resolve_push`], and sends the
//! result through the [`Remote`] returned by [`open_push_destination`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use thiserror::Error;

use crate::branch_tracking::{apply_push_set_upstream, remote_tracking_ref_for_remote_ref};
use crate::config::gitmodules::gitdir::is_git_directory;
use crate::config::{parse_git_config_maybe_bool, ConfigSet};
use crate::merge_base::is_ancestor;
use crate::objects::{ObjectId, ObjectKind};
use crate::push_report::PushRefResult;
use crate::refs;
use crate::refspec::{parse_push_refspec, RefspecItem};
use crate::remote::{
    HttpClientFactory, PreflightHttpClientFactory, Remote, RemoteError, RemoteRef,
};
use crate::repo::Repository;
use crate::repo_handles::has_remote_section;
use crate::rev_parse::{resolve_push_full_ref_for_branch, resolve_revision_for_verify};
use crate::state::{resolve_head, HeadState};
use crate::transfer::{resolve_push_delete_ref, PushRefSpec};

pub use crate::remote::DEFAULT_REMOTE;

/// How non-fast-forward updates are allowed during a push.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ForceMode {
    /// Reject non-fast-forward updates (default).
    #[default]
    None,
    /// Unconditional force (`--force`).
    Force,
    /// Guarded force (`--force-with-lease`).
    Lease(LeaseExpectation),
}

/// What a force-with-lease push compares the remote ref against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseExpectation {
    /// Use the local remote-tracking ref mapped from the push destination.
    Tracking,
    /// Use an explicit object id (`--force-with-lease=<rev>`); only valid for a single ref.
    Value(ObjectId),
}

/// What the caller wants to push (remote override, refspecs, tags, deletions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushRequest {
    /// Explicit remote name from `--remote` or the first positional argument.
    pub remote: Option<String>,
    /// Positional refspec strings (`src`, `src:dst`, `:dst`, `+src:dst`, …).
    pub refspecs: Vec<String>,
    /// Push all local tags (`--tags`).
    pub tags: bool,
    /// Delete the remote refs named in [`Self::refspecs`] instead of pushing them (`--delete`).
    ///
    /// Every refspec must then be a plain ref name (no `:`); see
    /// [`PushPlanError::DeleteNotPlainName`].
    pub delete_refs: bool,
    /// Additional remote refs to delete, planned together with [`Self::refspecs`] as when
    /// [`Self::delete_refs`] is set.
    pub delete: Vec<String>,
    /// Force or force-with-lease mode from CLI flags.
    pub force: ForceMode,
    /// Push annotated tags reachable from the commits being pushed (`--follow-tags`).
    pub follow_tags: bool,
    /// After a successful branch push, write `branch.<name>.remote` / `.merge` (`-u`).
    pub set_upstream: bool,
    /// Push every local branch (`refs/heads/*:refs/heads/*`), including names the remote
    /// does not advertise yet (unlike `push.default = matching` or a `:` refspec).
    pub all: bool,
    /// Mirror the repository: push all refs under `refs/*`, force updates, and delete remote
    /// refs under `refs/` with no local counterpart (implies [`Self::prune`] and force).
    pub mirror: bool,
    /// Delete remote refs in the destination namespace of a wildcard or matching refspec when
    /// there is no matching update in this push (for example `grit push --all --prune` or
    /// `refs/heads/*:refs/heads/* --prune`). A single-branch push does not prune siblings.
    pub prune: bool,
}

/// A fully resolved push target.
#[derive(Debug, Clone)]
pub struct ResolvedPush {
    /// Remote name to push to.
    pub remote: String,
    /// Concrete ref updates to send.
    pub specs: Vec<PushRefSpec>,
    /// Short branch label when the push maps to one local branch (for display).
    pub branch_label: Option<String>,
    /// High-level kind for JSON (`branch`, `tags`, `delete`, `refspecs`).
    pub kind: PushPlanKind,
}

/// JSON / CLI classification of what was pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushPlanKind {
    /// Default push of the current branch (or a single mapped branch).
    Branch,
    /// Push all local tags (`--tags`).
    Tags,
    /// Delete one or more remote refs (`--delete`).
    Delete,
    /// Explicit refspecs from the command line or remote `push` config.
    Refspecs,
}

/// Extra inputs needed to DWIM delete targets and short push destinations.
#[derive(Debug, Clone, Default)]
pub struct PushPlanContext {
    /// Advertised ref names on the remote (for delete mapping and dst guessing).
    pub remote_ref_names: HashSet<String>,
    /// Tips of `refs/heads/*` on the remote (reserved for push planning; follow-tags
    /// uses commit-graph ancestor queries instead of materializing reachable sets).
    pub remote_branch_tips: HashSet<ObjectId>,
    /// Tip of every advertised ref, or `None` when the advertisement was not read.
    ///
    /// Pass it on as [`PushOptions::remote_ref_tips`](crate::transfer::PushOptions) so a local
    /// push does not scan the remote's refs a second time. Only pass it to a push to the same
    /// remote: [`load_push_plan_context`] and [`resolve_push`] select the same remote when given
    /// the same override.
    pub remote_tips: Option<Vec<ObjectId>>,
}

/// Errors while planning a push (typed; no stringly matching in callers).
#[derive(Debug, Error)]
pub enum PushPlanError {
    /// Detached `HEAD` with no refspec on the command line.
    #[error("HEAD is detached; specify a refspec (for example HEAD:refs/heads/<name>)")]
    DetachedHeadNoRefspec,
    /// Current branch exists but has no commits.
    #[error("no commits yet to push")]
    NoCommits,
    /// `--tags` was given but there are no local tags.
    #[error("no tags to push")]
    NoTags,
    /// `push.default = nothing` blocks an empty refspec push.
    #[error("push.default is nothing; no push destination")]
    PushDefaultNothing,
    /// No remote was named and none is configured: the branch has no push remote or remote,
    /// `remote.pushDefault` is unset and there is no `origin` with a URL.
    #[error("no remote to push to")]
    NoRemote,
    /// Named remote is missing from config.
    #[error("unknown remote '{0}'")]
    UnknownRemote(String),
    /// Malformed refspec string.
    #[error("invalid refspec '{spec}': {reason}")]
    InvalidRefspec {
        /// Original refspec text.
        spec: String,
        /// Parse or validation detail.
        reason: String,
    },
    /// Source side did not match any local ref or object.
    #[error("src refspec {spec} does not match any ref")]
    SrcRefspecNoMatch {
        /// Source pattern or name.
        spec: String,
    },
    /// Source side matched more than one local ref.
    #[error("src refspec {spec} matches more than one ref")]
    SrcRefspecAmbiguous {
        /// Source pattern or name.
        spec: String,
    },
    /// Short destination matched multiple remote refs.
    #[error("dst refspec {spec} matches more than one remote ref")]
    DstRefspecAmbiguous {
        /// Destination pattern or name.
        spec: String,
    },
    /// Revision text could not be resolved to an object.
    #[error("could not resolve revision '{spec}'")]
    UnknownRevision {
        /// Revision expression.
        spec: String,
    },
    /// `--force-with-lease=<rev>` with more than one ref update.
    #[error("--force-with-lease=<rev> requires pushing exactly one ref")]
    LeaseValueRequiresSingleRef,
    /// A delete push was given a refspec (`src:dst`) instead of a plain remote ref name.
    #[error("cannot delete '{spec}': --delete takes ref names, not src:dst refspecs")]
    DeleteNotPlainName {
        /// The offending refspec.
        spec: String,
    },
    /// A delete push named no ref to delete.
    #[error("--delete needs at least one ref to delete")]
    DeleteWithoutRefs,
    /// `HEAD` could not be interpreted for push planning.
    #[error("HEAD is in an unknown state")]
    InvalidHead,
    /// `push.default = upstream` but the branch has no `branch.<name>.merge`.
    #[error("branch '{branch}' has no upstream for push.default upstream")]
    NoUpstreamForPushDefault {
        /// Local branch short name.
        branch: String,
    },
    /// Repository failure while planning the push.
    #[error(transparent)]
    Nested(#[from] Box<crate::error::Error>),
}

type PlanResult<T> = std::result::Result<T, PushPlanError>;

impl From<crate::error::Error> for PushPlanError {
    fn from(e: crate::error::Error) -> Self {
        Self::Nested(Box::new(e))
    }
}

/// Parse `--force` / `--force-with-lease[=<rev>]` into [`ForceMode`].
pub fn parse_force_mode(
    repo: &Repository,
    force: bool,
    force_with_lease: Option<Option<String>>,
) -> PlanResult<ForceMode> {
    if force {
        return Ok(ForceMode::Force);
    }
    let Some(flag) = force_with_lease else {
        return Ok(ForceMode::None);
    };
    match flag {
        None => Ok(ForceMode::Lease(LeaseExpectation::Tracking)),
        Some(rev) => {
            let oid = crate::rev_parse::resolve_revision_for_verify(repo, &rev)
                .map_err(|_| PushPlanError::UnknownRevision { spec: rev.clone() })?;
            Ok(ForceMode::Lease(LeaseExpectation::Value(oid)))
        }
    }
}

/// Resolve `request` into a remote name and [`PushRefSpec`] list.
///
/// When `ctx.remote_ref_names` is empty, delete/ref DWIM still uses local refs
/// and falls back to Git's default `refs/heads/<name>` for deletes.
///
/// A delete push ([`PushRequest::delete_refs`] or a non-empty [`PushRequest::delete`]) plans one
/// deletion per name in `delete` and `refspecs`, and nothing else.
///
/// # Errors
///
/// Besides remote and refspec resolution failures: [`PushPlanError::DeleteNotPlainName`] when a
/// delete push is given a `src:dst` refspec, and [`PushPlanError::DeleteWithoutRefs`] when it
/// names no ref. Both are reported before the remote is contacted.
pub fn resolve_push(
    repo: &Repository,
    request: &PushRequest,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let config = repo.config().snapshot().map_err(PushPlanError::from)?;
    if request.delete_refs || !request.delete.is_empty() {
        return resolve_delete_push(repo, config.as_ref(), request, ctx);
    }
    if request.tags {
        return resolve_tags_push(repo, config.as_ref(), request);
    }

    let remote = configured_remote_for_push(repo, config.as_ref(), request.remote.as_deref())?;
    validate_remote(config.as_ref(), &remote)?;

    let follow_tags = request.follow_tags || push_follow_tags_config(config.as_ref());

    let force_mode = if request.mirror {
        ForceMode::Force
    } else {
        request.force.clone()
    };

    let mut resolved = if request.mirror {
        resolve_mirror_push(repo, &remote, ctx)?
    } else if request.all && request.refspecs.is_empty() {
        resolve_all_branches_push(repo, &remote, ctx)?
    } else if request.refspecs.is_empty() {
        resolve_default_push(repo, config.as_ref(), &remote, ctx)?
    } else {
        resolve_cli_refspecs_push(repo, config.as_ref(), &remote, &request.refspecs, ctx)?
    };

    if request.prune && !request.mirror {
        let scopes = collect_prune_scopes(config.as_ref(), &remote, request);
        let negatives = collect_prune_negative_refspecs(config.as_ref(), &remote, request);
        apply_prune_scopes(&mut resolved.specs, ctx, &scopes, &negatives);
    }

    if follow_tags {
        let extra = follow_tag_push_specs(repo, &resolved.specs, ctx)?;
        if !extra.is_empty() {
            resolved.specs.extend(extra);
            resolved.kind = PushPlanKind::Refspecs;
            resolved.branch_label = None;
        }
    }

    apply_force_mode(repo, &resolved.remote, &mut resolved.specs, &force_mode)?;
    Ok(resolved)
}

fn resolve_all_branches_push(
    repo: &Repository,
    remote: &str,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let local = LocalPushRefs::load(repo)?;
    let item = parse_push_refspec("refs/heads/*:refs/heads/*").map_err(|e| {
        PushPlanError::InvalidRefspec {
            spec: "refs/heads/*:refs/heads/*".into(),
            reason: e.to_string(),
        }
    })?;
    let specs = expand_wildcard_push_item(repo, &item, ctx, &local)?;
    if specs.is_empty() {
        return Err(PushPlanError::SrcRefspecNoMatch {
            spec: "refs/heads/*".into(),
        });
    }
    let branch_label = branch_label_from_specs(repo, &specs);
    let kind = if specs.len() == 1 {
        PushPlanKind::Branch
    } else {
        PushPlanKind::Refspecs
    };
    Ok(ResolvedPush {
        remote: remote.to_owned(),
        specs,
        branch_label,
        kind,
    })
}

fn resolve_mirror_push(
    repo: &Repository,
    remote: &str,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let local = LocalPushRefs::load(repo)?;
    let item = parse_push_refspec("refs/*:refs/*").map_err(|e| PushPlanError::InvalidRefspec {
        spec: "refs/*:refs/*".into(),
        reason: e.to_string(),
    })?;
    let mut specs = expand_wildcard_push_item(repo, &item, ctx, &local)?;
    if specs.is_empty() {
        return Err(PushPlanError::SrcRefspecNoMatch {
            spec: "refs/*".into(),
        });
    }
    let local_dst: HashSet<String> = specs.iter().map(|s| s.dst.clone()).collect();
    append_mirror_prune_deletes(&mut specs, ctx, &local_dst);
    Ok(ResolvedPush {
        remote: remote.to_owned(),
        specs,
        branch_label: None,
        kind: PushPlanKind::Refspecs,
    })
}

fn resolve_cli_refspecs_push(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    refspecs: &[String],
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let local = LocalPushRefs::load(repo)?;
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for spec in refspecs {
        let item = parse_push_refspec(spec).map_err(|e| PushPlanError::InvalidRefspec {
            spec: spec.clone(),
            reason: e.to_string(),
        })?;
        if item.negative {
            negative.push(item);
        } else {
            positive.push(spec.as_str());
        }
    }
    let mut specs = Vec::new();
    for spec in positive {
        specs.extend(resolve_cli_refspec(
            repo, config, remote, spec, ctx, &local,
        )?);
    }
    for item in negative {
        specs.retain(|s| !push_spec_excluded_by_negative(&item, s));
    }
    let branch_label = branch_label_from_specs(repo, &specs);
    Ok(ResolvedPush {
        remote: remote.to_owned(),
        specs,
        branch_label,
        kind: PushPlanKind::Refspecs,
    })
}

fn positive_prune_scopes_from_refspec_strings(specs: &[String]) -> Vec<RefspecItem> {
    specs
        .iter()
        .filter_map(|spec| {
            let item = parse_push_refspec(spec).ok()?;
            if item.negative || (!item.pattern && !item.matching) {
                return None;
            }
            Some(item)
        })
        .collect()
}

/// Wildcard or matching refspecs that define `--prune` destination namespaces.
fn collect_prune_scopes(
    config: &ConfigSet,
    remote: &str,
    request: &PushRequest,
) -> Vec<RefspecItem> {
    if request.all {
        return parse_push_refspec("refs/heads/*:refs/heads/*")
            .ok()
            .into_iter()
            .collect();
    }
    if !request.refspecs.is_empty() {
        return positive_prune_scopes_from_refspec_strings(&request.refspecs);
    }
    let configured = remote_push_refspec_strings(config, remote);
    let mut scopes = positive_prune_scopes_from_refspec_strings(&configured);
    if scopes.is_empty() {
        let push_default = config
            .value("push.default")
            .unwrap_or_else(|| "simple".to_owned());
        if push_default == "matching" {
            if let Ok(item) = parse_push_refspec(":") {
                scopes.push(item);
            }
        }
    }
    scopes
}

fn negative_refspecs_from_strings(specs: &[String]) -> Vec<RefspecItem> {
    specs
        .iter()
        .filter_map(|spec| {
            let item = parse_push_refspec(spec).ok()?;
            item.negative.then_some(item)
        })
        .collect()
}

fn collect_prune_negative_refspecs(
    config: &ConfigSet,
    remote: &str,
    request: &PushRequest,
) -> Vec<RefspecItem> {
    if !request.refspecs.is_empty() {
        return negative_refspecs_from_strings(&request.refspecs);
    }
    negative_refspecs_from_strings(&remote_push_refspec_strings(config, remote))
}

fn apply_prune_scopes(
    specs: &mut Vec<PushRefSpec>,
    ctx: &PushPlanContext,
    scopes: &[RefspecItem],
    negatives: &[RefspecItem],
) {
    for scope in scopes {
        append_prune_deletes_for_scope(specs, ctx, scope, negatives);
    }
}

fn prune_destination_patterns(scope: &RefspecItem) -> Vec<String> {
    if scope.matching {
        return vec!["refs/heads/*".to_owned()];
    }
    if scope.pattern {
        if let Some(dst) = scope.dst.as_deref().filter(|d| d.contains('*')) {
            return vec![dst.to_owned()];
        }
        if let Some(src) = scope.src.as_deref().filter(|s| s.contains('*')) {
            return vec![src.to_owned()];
        }
    }
    Vec::new()
}

fn remote_ref_in_prune_destination_namespace(remote_ref: &str, dst_pattern: &str) -> bool {
    if !dst_pattern.contains('*') {
        return remote_ref == dst_pattern;
    }
    match_refname_with_pattern(dst_pattern, remote_ref, dst_pattern).is_some()
}

fn prune_scope_local_ref_for_remote_dst(remote_ref: &str, scope: &RefspecItem) -> Option<String> {
    if scope.matching {
        return remote_ref
            .starts_with("refs/heads/")
            .then(|| remote_ref.to_owned());
    }
    if scope.pattern {
        let src_pat = scope.src.as_deref()?;
        let dst_pat = scope.dst.as_deref()?;
        return match_refname_with_pattern(dst_pat, remote_ref, src_pat);
    }
    None
}

fn negative_excludes_local_ref(negative: &RefspecItem, local_ref: &str) -> bool {
    let Some(pat) = negative.src.as_deref() else {
        return false;
    };
    if negative.pattern {
        return match_refname_with_pattern(pat, local_ref, pat).is_some();
    }
    pat == local_ref || refname_match_one(pat, local_ref)
}

fn prune_dst_excluded_by_negative(
    negative: &RefspecItem,
    remote_ref: &str,
    scope: &RefspecItem,
) -> bool {
    let Some(local) = prune_scope_local_ref_for_remote_dst(remote_ref, scope) else {
        return false;
    };
    negative_excludes_local_ref(negative, &local)
}

fn append_prune_deletes_for_scope(
    specs: &mut Vec<PushRefSpec>,
    ctx: &PushPlanContext,
    scope: &RefspecItem,
    negatives: &[RefspecItem],
) {
    let patterns = prune_destination_patterns(scope);
    if patterns.is_empty() {
        return;
    }
    let pushed_dst: HashSet<&str> = specs
        .iter()
        .filter(|s| !s.delete)
        .map(|s| s.dst.as_str())
        .collect();
    let mut to_delete = Vec::new();
    for remote_ref in &ctx.remote_ref_names {
        if remote_ref == "HEAD" {
            continue;
        }
        if !patterns
            .iter()
            .any(|pat| remote_ref_in_prune_destination_namespace(remote_ref, pat))
        {
            continue;
        }
        if pushed_dst.contains(remote_ref.as_str()) {
            continue;
        }
        if specs.iter().any(|s| s.dst == *remote_ref) {
            continue;
        }
        if negatives
            .iter()
            .any(|n| prune_dst_excluded_by_negative(n, remote_ref, scope))
        {
            continue;
        }
        to_delete.push(remote_ref.clone());
    }
    for remote_ref in to_delete {
        specs.push(push_spec(None, None, remote_ref, false, true));
    }
}

fn append_mirror_prune_deletes(
    specs: &mut Vec<PushRefSpec>,
    ctx: &PushPlanContext,
    local_dst: &HashSet<String>,
) {
    let mut to_delete = Vec::new();
    for remote_ref in &ctx.remote_ref_names {
        if remote_ref == "HEAD" || !remote_ref.starts_with("refs/") {
            continue;
        }
        if local_dst.contains(remote_ref) {
            continue;
        }
        if specs.iter().any(|s| s.dst == *remote_ref) {
            continue;
        }
        to_delete.push(remote_ref.clone());
    }
    for remote_ref in to_delete {
        specs.push(push_spec(None, None, remote_ref, false, true));
    }
}

/// Whether `push.followTags` is enabled in config (default false).
#[must_use]
pub fn push_follow_tags_config(config: &ConfigSet) -> bool {
    config
        .value("push.followTags")
        .and_then(|v| parse_git_config_maybe_bool(&v))
        .unwrap_or(false)
}

/// Append annotated tag refspecs reachable from the updates in `specs`.
///
/// Matches `git push --follow-tags`: lightweight tags are skipped, tags already on the remote
/// are skipped, and reachability uses commit-graph ancestor checks from pushed tips (any ref
/// namespace). Tag refspecs and specs whose source is a tag object do not seed the walk.
pub fn follow_tag_push_specs(
    repo: &Repository,
    specs: &[PushRefSpec],
    ctx: &PushPlanContext,
) -> PlanResult<Vec<PushRefSpec>> {
    let tips = push_follow_tip_commits(repo, specs)?;
    if tips.is_empty() {
        return Ok(Vec::new());
    }

    let existing: HashSet<&str> = specs.iter().map(|s| s.dst.as_str()).collect();
    let odb = repo.odb();
    let mut out = Vec::new();
    for (tag_name, tag_oid) in list_local_refs(repo.git_dir(), "refs/tags/")? {
        if ctx.remote_ref_names.contains(&tag_name) || existing.contains(tag_name.as_str()) {
            continue;
        }
        let Ok(obj) = odb.read(&tag_oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Tag {
            continue;
        }
        let Ok((peeled, _)) = odb.peel_tags(tag_oid) else {
            continue;
        };
        if !tag_reachable_from_push_tips(repo, peeled, &tips)? {
            continue;
        }
        out.push(push_spec(
            Some(tag_oid),
            Some(tag_name.clone()),
            tag_name,
            false,
            false,
        ));
    }
    Ok(out)
}

/// After a successful push, apply `-u` / `--set-upstream` when [`PushRequest::set_upstream`] was set.
///
/// Returns the short names of branches whose upstream config was written. No-op when `set_upstream`
/// is false or `dry_run` is true.
pub fn apply_set_upstream_after_push(
    repo: &Repository,
    remote: &str,
    set_upstream: bool,
    dry_run: bool,
    specs: &[PushRefSpec],
    results: &[PushRefResult],
) -> Result<Vec<String>, crate::error::Error> {
    if !set_upstream || dry_run {
        return Ok(Vec::new());
    }
    apply_push_set_upstream(repo, remote, specs, results)
}

fn push_follow_tip_commits(repo: &Repository, specs: &[PushRefSpec]) -> PlanResult<Vec<ObjectId>> {
    let mut tips = Vec::new();
    for spec in specs {
        if spec.delete || is_tag_push_spec(repo, spec) {
            continue;
        }
        if let Some(commit) = peel_push_src_to_commit(repo, spec) {
            tips.push(commit);
        }
    }
    Ok(tips)
}

fn is_tag_push_spec(repo: &Repository, spec: &PushRefSpec) -> bool {
    if spec.dst.starts_with("refs/tags/") {
        return true;
    }
    let Some(src) = spec.src else {
        return false;
    };
    let Ok(obj) = repo.odb().read(&src) else {
        return false;
    };
    obj.kind == ObjectKind::Tag
}

fn peel_push_src_to_commit(repo: &Repository, spec: &PushRefSpec) -> Option<ObjectId> {
    let src = spec.src?;
    let odb = repo.odb();
    let Ok(obj) = odb.read(&src) else {
        return None;
    };
    match obj.kind {
        ObjectKind::Commit => Some(src),
        ObjectKind::Tag => odb.peel_tags(src).ok().map(|(commit, _)| commit),
        _ => None,
    }
}

fn tag_reachable_from_push_tips(
    repo: &Repository,
    peeled: ObjectId,
    tips: &[ObjectId],
) -> PlanResult<bool> {
    for &tip in tips {
        if peeled == tip {
            return Ok(true);
        }
        if is_ancestor(repo, peeled, tip).map_err(|e| PushPlanError::Nested(Box::new(e)))? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_delete_push(
    repo: &Repository,
    config: &ConfigSet,
    request: &PushRequest,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let names: Vec<&String> = request.delete.iter().chain(&request.refspecs).collect();
    if let Some(spec) = names.iter().find(|name| name.contains(':')) {
        return Err(PushPlanError::DeleteNotPlainName {
            spec: (*spec).clone(),
        });
    }
    if names.is_empty() {
        return Err(PushPlanError::DeleteWithoutRefs);
    }
    let remote = configured_remote_for_push(repo, config, request.remote.as_deref())?;
    validate_remote(config, &remote)?;
    let specs = names
        .into_iter()
        .map(|name| {
            let dst = resolve_push_delete_ref(repo.git_dir(), &ctx.remote_ref_names, name);
            push_spec(None, None, dst, false, true)
        })
        .collect::<Vec<_>>();
    let branch_label = delete_branch_label(&specs);
    let mut resolved = ResolvedPush {
        remote,
        specs,
        branch_label,
        kind: PushPlanKind::Delete,
    };
    apply_force_mode(repo, &resolved.remote, &mut resolved.specs, &request.force)?;
    Ok(resolved)
}

fn resolve_tags_push(
    repo: &Repository,
    config: &ConfigSet,
    request: &PushRequest,
) -> PlanResult<ResolvedPush> {
    let remote = configured_remote_for_push(repo, config, request.remote.as_deref())?;
    validate_remote(config, &remote)?;
    let entries = refs::list_refs(repo.git_dir(), "refs/tags/")
        .map_err(|e| PushPlanError::Nested(Box::new(e)))?;
    if entries.is_empty() {
        return Err(PushPlanError::NoTags);
    }
    let specs = entries
        .into_iter()
        .map(|(refname, oid)| push_spec(Some(oid), Some(refname.clone()), refname, false, false))
        .collect();
    let mut resolved = ResolvedPush {
        remote,
        specs,
        branch_label: None,
        kind: PushPlanKind::Tags,
    };
    apply_force_mode(repo, &resolved.remote, &mut resolved.specs, &request.force)?;
    Ok(resolved)
}

fn resolve_default_push(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let push_specs = remote_push_refspec_strings(config, remote);
    if !push_specs.is_empty() {
        let mut positive_specs = Vec::new();
        let mut negative_items = Vec::new();
        for spec in push_specs {
            let item = parse_push_refspec(&spec).map_err(|e| PushPlanError::InvalidRefspec {
                spec: spec.clone(),
                reason: e.to_string(),
            })?;
            if item.negative {
                negative_items.push(item);
            } else {
                positive_specs.push(spec);
            }
        }
        let local = LocalPushRefs::load(repo)?;
        let mut specs = Vec::new();
        for spec in positive_specs {
            specs.extend(expand_configured_push_refspec(repo, spec, ctx, &local)?);
        }
        for item in negative_items {
            specs.retain(|s| !push_spec_excluded_by_negative(&item, s));
        }
        let branch_label = branch_label_from_specs(repo, &specs);
        let kind = if specs.len() == 1 {
            PushPlanKind::Branch
        } else {
            PushPlanKind::Refspecs
        };
        return Ok(ResolvedPush {
            remote: remote.to_owned(),
            specs,
            branch_label,
            kind,
        });
    }

    let push_default = config
        .value("push.default")
        .unwrap_or_else(|| "simple".to_owned());
    if push_default == "matching" {
        return resolve_matching_default_push(repo, remote, ctx);
    }

    let (short_name, oid) = current_branch_oid(repo)?;
    let src_ref = format!("refs/heads/{short_name}");
    let dst = default_push_destination(repo, config, remote, &short_name, &src_ref)?;
    let spec = push_spec(Some(oid), Some(src_ref.clone()), dst, false, false);
    Ok(ResolvedPush {
        remote: remote.to_owned(),
        specs: vec![spec],
        branch_label: Some(short_name),
        kind: PushPlanKind::Branch,
    })
}

/// `push.default = matching`: push every local branch whose name exists on the remote.
fn resolve_matching_default_push(
    repo: &Repository,
    remote: &str,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let heads = list_local_refs(repo.git_dir(), "refs/heads/")?;
    let mut specs = Vec::new();
    for (name, oid) in heads {
        if ctx.remote_ref_names.contains(&name) {
            specs.push(push_spec(
                Some(oid),
                Some(name.clone()),
                name.clone(),
                false,
                false,
            ));
        }
    }
    let branch_label = branch_label_from_specs(repo, &specs);
    let kind = if specs.len() == 1 {
        PushPlanKind::Branch
    } else {
        PushPlanKind::Refspecs
    };
    Ok(ResolvedPush {
        remote: remote.to_owned(),
        specs,
        branch_label,
        kind,
    })
}

fn current_branch_oid(repo: &Repository) -> PlanResult<(String, ObjectId)> {
    match resolve_head(repo.git_dir()).map_err(|e| PushPlanError::Nested(Box::new(e)))? {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => Ok((short_name, oid)),
        HeadState::Branch { .. } => Err(PushPlanError::NoCommits),
        HeadState::Detached { .. } => Err(PushPlanError::DetachedHeadNoRefspec),
        HeadState::Invalid => Err(PushPlanError::InvalidHead),
    }
}

/// Remote selection for push: explicit, `branch.*.pushRemote`, `remote.pushDefault`,
/// `branch.*.remote`, then `origin`.
///
/// # Errors
///
/// [`PushPlanError::NoRemote`] when nothing selects a remote and no `remote.origin.*` section
/// is configured.
pub(crate) fn configured_remote_for_push(
    repo: &Repository,
    config: &ConfigSet,
    remote_override: Option<&str>,
) -> PlanResult<String> {
    if let Some(remote) = remote_override.filter(|r| !r.trim().is_empty()) {
        return Ok(remote.to_owned());
    }
    if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
        if let Some(remote) = config
            .value(&format!("branch.{short_name}.pushremote"))
            .or_else(|| config.value(&format!("branch.{short_name}.pushRemote")))
            .filter(|r| !r.trim().is_empty())
        {
            return Ok(remote);
        }
    }
    if let Some(remote) = config
        .value("remote.pushDefault")
        .filter(|r| !r.trim().is_empty())
    {
        return Ok(remote);
    }
    if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
        if let Some(remote) = config
            .value(&format!("branch.{short_name}.remote"))
            .filter(|r| !r.trim().is_empty() && r != ".")
        {
            return Ok(remote);
        }
    }
    if !has_remote_section(config, DEFAULT_REMOTE) {
        return Err(PushPlanError::NoRemote);
    }
    Ok(DEFAULT_REMOTE.to_owned())
}

/// Remote and refspec strings after classifying `grit push [<remote>] [<refspec>...]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushInvocation {
    /// Remote name, URL or path; `None` selects the remote with [`configured_remote_for_push`].
    pub remote: Option<String>,
    /// Refspec strings for [`PushRequest::refspecs`].
    pub refspecs: Vec<String>,
}

/// Classify push positionals into a remote and refspecs.
///
/// With `remote_flag` set, every positional is a refspec. Otherwise the first positional is the
/// remote when it has a configured `remote.<name>.url` or looks like a literal URL or path
/// (`scheme://`, `*.git`, `/abs/path`, `.` / `..`, `./…` / `../…`, or an existing relative git
/// directory). A single other positional is a refspec pushed to the default remote; with more
/// positionals the first one must name a remote.
///
/// # Errors
///
/// [`PushPlanError::UnknownRemote`] when several positionals are given and the first is neither a
/// configured remote nor a literal URL or path.
pub fn resolve_push_invocation(
    config: &ConfigSet,
    remote_flag: Option<&str>,
    positionals: &[String],
) -> PlanResult<PushInvocation> {
    if let Some(remote) = remote_flag {
        return Ok(PushInvocation {
            remote: Some(remote.to_owned()),
            refspecs: positionals.to_vec(),
        });
    }
    let Some((first, rest)) = positionals.split_first() else {
        return Ok(PushInvocation::default());
    };
    if has_remote_url(config, first) || looks_like_literal_remote(first) {
        return Ok(PushInvocation {
            remote: Some(first.clone()),
            refspecs: rest.to_vec(),
        });
    }
    if !rest.is_empty() {
        return Err(PushPlanError::UnknownRemote(first.clone()));
    }
    Ok(PushInvocation {
        remote: None,
        refspecs: positionals.to_vec(),
    })
}

/// The URL `<host>/<owner>/<project>.git` when `grit push <owner>/<project> <refspec>...` names
/// a repository on a grit host rather than a remote.
///
/// Only a first positional followed by refspecs is a remote, so a lone `topic/x` stays a
/// refspec (`None`). A configured remote of the same name wins, and literal URLs and paths
/// (`a/b/c`, `./a/b`, `x/y.git`) are left alone. Owner and project are letters, digits, `-`,
/// `_` and `.`, not starting with `.`.
#[must_use]
pub fn expand_repository_shorthand(
    config: &ConfigSet,
    positionals: &[String],
    host: &str,
) -> Option<String> {
    let [first, _, ..] = positionals else {
        return None;
    };
    if has_remote_url(config, first) || looks_like_literal_remote(first) {
        return None;
    }
    let is_segment = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let (owner, project) = first.split_once('/')?;
    (is_segment(owner) && is_segment(project))
        .then(|| format!("{}/{owner}/{project}.git", host.trim_end_matches('/')))
}

/// Open the [`Remote`] a resolved push sends to ([`ResolvedPush::remote`]).
///
/// Literal URLs and paths (see [`resolve_push_invocation`]) become anonymous remotes; any other
/// name is read from `remote.<name>.*` config.
///
/// # Errors
///
/// [`RemoteError::NoUrl`] when `name` has no configured URL, or a URL parse error.
pub fn open_push_destination(config: &ConfigSet, name: &str) -> Result<Remote, RemoteError> {
    if looks_like_literal_remote(name) {
        Remote::from_url(name)
    } else {
        Remote::from_config(config, name)
    }
}

impl PushPlanContext {
    /// Build a context from a remote's ref advertisement (peeled `^{}` entries are skipped).
    #[must_use]
    pub fn from_advertised_refs(refs: impl IntoIterator<Item = RemoteRef>) -> Self {
        let mut ctx = Self::default();
        let mut tips = Vec::new();
        for remote_ref in refs {
            if remote_ref.peeled_tag().is_some() {
                continue;
            }
            if remote_ref.name.starts_with("refs/heads/") {
                ctx.remote_branch_tips.insert(remote_ref.oid);
            }
            tips.push(remote_ref.oid);
            ctx.remote_ref_names.insert(remote_ref.name);
        }
        ctx.remote_tips = Some(tips);
        ctx
    }
}

/// Load the [`PushPlanContext`] for the remote a push would select.
///
/// The remote is chosen with [`configured_remote_for_push`], opened with
/// [`open_push_destination`] (so literal URLs and paths are listed like configured remotes) and
/// listed with [`Remote::list_push_refs`]. The context is best-effort: a remote without a URL, or one whose refs cannot be listed,
/// yields an empty context, and the push itself reports any connection failure.
///
/// The listing uses a [preflight client](HttpClientFactory::create_for_preflight), so credentials the
/// server refuses stay stored and the push reports them as rejected
/// ([`AuthFailure::Rejected`](crate::error::AuthFailure::Rejected)).
///
/// # Errors
///
/// Only errors from [`configured_remote_for_push`].
pub fn load_push_plan_context(
    repo: &Repository,
    remote_override: Option<&str>,
    http_factory: Option<&dyn HttpClientFactory>,
) -> PlanResult<PushPlanContext> {
    let config = repo.config().snapshot().map_err(PushPlanError::from)?;
    let remote = configured_remote_for_push(repo, config.as_ref(), remote_override)?;
    let Ok(handle) = open_push_destination(config.as_ref(), &remote) else {
        return Ok(PushPlanContext::default());
    };
    let probe = PreflightHttpClientFactory::new(http_factory, repo);
    let Ok(advertised) = handle.list_push_refs(repo, Some(&probe)) else {
        return Ok(PushPlanContext::default());
    };
    Ok(PushPlanContext::from_advertised_refs(advertised))
}

fn looks_like_literal_remote(name: &str) -> bool {
    if name.contains("://") || name.ends_with(".git") || name.starts_with('/') {
        return true;
    }
    if name == "." || name == ".." || name.starts_with("./") || name.starts_with("../") {
        return true;
    }
    is_git_directory(Path::new(name))
}

fn has_remote_url(config: &ConfigSet, name: &str) -> bool {
    config
        .value(&format!("remote.{name}.url"))
        .is_some_and(|u| !u.trim().is_empty())
}

fn validate_remote(config: &ConfigSet, name: &str) -> PlanResult<()> {
    if has_remote_url(config, name) {
        Ok(())
    } else if name == DEFAULT_REMOTE || looks_like_literal_remote(name) {
        // Literal URL / path remotes are resolved by the transport layer.
        Ok(())
    } else {
        Err(PushPlanError::UnknownRemote(name.to_owned()))
    }
}

fn remote_push_refspec_strings(config: &ConfigSet, remote: &str) -> Vec<String> {
    config
        .values_all(&format!("remote.{remote}.push"))
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect()
}

fn default_push_destination(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    branch_short: &str,
    _src_ref: &str,
) -> PlanResult<String> {
    let push_default = config
        .value("push.default")
        .unwrap_or_else(|| "simple".to_owned());

    if push_default == "nothing" {
        return Err(PushPlanError::PushDefaultNothing);
    }

    if push_default.as_str() == "upstream" {
        let merge = config
            .value(&format!("branch.{branch_short}.merge"))
            .filter(|m| m.starts_with("refs/"));
        return merge.ok_or(PushPlanError::NoUpstreamForPushDefault {
            branch: branch_short.to_owned(),
        });
    }

    // `simple`, `current`, and unspecified: grit keeps the historical default of
    // `branch.<name>.merge` when set, else same branch name on the remote.
    if push_default.as_str() == "simple" {
        let branch_remote = config.value(&format!("branch.{branch_short}.remote"));
        if branch_remote.as_deref() == Some(remote) {
            if let Some(merge) = config
                .value(&format!("branch.{branch_short}.merge"))
                .filter(|m| m.starts_with("refs/"))
            {
                let _merge_branch = merge.strip_prefix("refs/heads/").unwrap_or(merge.as_str());
                let same_name_dst = format!("refs/heads/{branch_short}");
                if merge != same_name_dst {
                    // Git `simple` dies when upstream name differs; grit uses merge ref when same remote.
                    return Ok(merge);
                }
                return Ok(same_name_dst);
            }
        }
    }

    let _ = resolve_push_full_ref_for_branch(repo, branch_short);
    Ok(format!("refs/heads/{branch_short}"))
}

fn resolve_cli_refspec(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    spec: &str,
    ctx: &PushPlanContext,
    local: &LocalPushRefs,
) -> PlanResult<Vec<PushRefSpec>> {
    if !spec.contains(':') {
        return resolve_shorthand_refspec(repo, config, remote, spec, ctx, local);
    }
    let item = parse_push_refspec(spec).map_err(|e| PushPlanError::InvalidRefspec {
        spec: spec.to_owned(),
        reason: e.to_string(),
    })?;
    resolve_parsed_refspec(repo, &item, ctx, local)
}

fn resolve_shorthand_refspec(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    spec: &str,
    ctx: &PushPlanContext,
    local: &LocalPushRefs,
) -> PlanResult<Vec<PushRefSpec>> {
    let count = local.count_refname_match(spec);
    if count == 1 {
        let Some(matched) = local.refname_match(spec) else {
            return Err(PushPlanError::SrcRefspecNoMatch {
                spec: spec.to_owned(),
            });
        };
        if let Some(mapped) = map_through_remote_push_refspec(config, remote, &matched.0) {
            let item = parse_push_refspec(&mapped).map_err(|e| PushPlanError::InvalidRefspec {
                spec: mapped.clone(),
                reason: e.to_string(),
            })?;
            return resolve_parsed_refspec(repo, &item, ctx, local);
        }
        let item = RefspecItem {
            force: false,
            src: Some(matched.0.clone()),
            dst: Some(matched.0.clone()),
            ..Default::default()
        };
        return push_spec_from_parsed_item(repo, &item, ctx, matched);
    }
    if count == 0 {
        // Treat as explicit refspec string (Git refspec_append).
        let item = parse_push_refspec(spec).map_err(|e| PushPlanError::InvalidRefspec {
            spec: spec.to_owned(),
            reason: e.to_string(),
        })?;
        return resolve_parsed_refspec(repo, &item, ctx, local);
    }
    Err(PushPlanError::SrcRefspecAmbiguous {
        spec: spec.to_owned(),
    })
}

fn map_through_remote_push_refspec(
    config: &ConfigSet,
    remote: &str,
    local_full_ref: &str,
) -> Option<String> {
    for spec in remote_push_refspec_strings(config, remote) {
        let Ok(item) = parse_push_refspec(&spec) else {
            continue;
        };
        if item.negative || item.matching {
            continue;
        }
        if let Some(dst) = refspec_map_src(&item, local_full_ref) {
            let force = if item.force { "+" } else { "" };
            return Some(format!("{force}{local_full_ref}:{dst}"));
        }
    }
    None
}

fn expand_configured_push_refspec(
    repo: &Repository,
    spec: String,
    ctx: &PushPlanContext,
    local: &LocalPushRefs,
) -> PlanResult<Vec<PushRefSpec>> {
    let item = parse_push_refspec(&spec).map_err(|e| PushPlanError::InvalidRefspec {
        spec: spec.clone(),
        reason: e.to_string(),
    })?;
    if item.matching {
        let mut out = Vec::new();
        for (name, oid) in local.heads() {
            if !ctx.remote_ref_names.contains(name) {
                continue;
            }
            out.push(push_spec(
                Some(*oid),
                Some(name.clone()),
                name.clone(),
                item.force,
                false,
            ));
        }
        return Ok(out);
    }
    if item.pattern {
        return expand_wildcard_push_item(repo, &item, ctx, local);
    }
    resolve_parsed_refspec(repo, &item, ctx, local)
}

fn expand_wildcard_push_item(
    repo: &Repository,
    item: &RefspecItem,
    _ctx: &PushPlanContext,
    local: &LocalPushRefs,
) -> PlanResult<Vec<PushRefSpec>> {
    let refs = pattern_source_refs(repo, local, item.src.as_deref().unwrap_or(""))?;
    let mut out = Vec::new();
    for (name, oid) in refs {
        if let Some(dst) = refspec_map_src(item, &name) {
            out.push(push_spec(
                Some(oid),
                Some(name.clone()),
                dst,
                item.force,
                false,
            ));
        }
    }
    Ok(out)
}

fn pattern_source_refs(
    repo: &Repository,
    local: &LocalPushRefs,
    prefix: &str,
) -> PlanResult<Vec<(String, ObjectId)>> {
    Ok(if prefix.starts_with("refs/tags/") {
        local.tags().to_vec()
    } else if prefix.starts_with("refs/heads/") {
        local.heads().to_vec()
    } else {
        list_local_refs(repo.git_dir(), "refs/")?
            .into_iter()
            .filter(|(name, _)| name != "HEAD")
            .collect()
    })
}

fn resolve_parsed_refspec(
    repo: &Repository,
    item: &RefspecItem,
    ctx: &PushPlanContext,
    local: &LocalPushRefs,
) -> PlanResult<Vec<PushRefSpec>> {
    if item.negative {
        return Ok(Vec::new());
    }
    if item.pattern {
        let specs = expand_wildcard_push_item(repo, item, ctx, local)?;
        if specs.is_empty() {
            return Err(PushPlanError::SrcRefspecNoMatch {
                spec: item.src.clone().unwrap_or_default(),
            });
        }
        return Ok(specs);
    }
    if item.matching {
        return expand_configured_push_refspec(
            repo,
            if item.force {
                "+:".to_owned()
            } else {
                ":".to_owned()
            },
            ctx,
            local,
        );
    }

    let src_empty = item.src.as_deref().is_some_and(|s| s.is_empty());
    let src_name = item.src.as_deref().unwrap_or("");

    if src_empty || (src_name.is_empty() && item.dst.is_some()) {
        let dst = item
            .dst
            .as_ref()
            .ok_or_else(|| PushPlanError::InvalidRefspec {
                spec: String::new(),
                reason: "delete refspec missing destination".into(),
            })?;
        let resolved_dst = resolve_dst_name(repo, dst, None, ctx)?;
        return Ok(vec![push_spec(None, None, resolved_dst, item.force, true)]);
    }

    let src_oid = if item.exact_sha1 {
        ObjectId::from_hex(src_name).map_err(|_| PushPlanError::UnknownRevision {
            spec: src_name.to_owned(),
        })?
    } else if src_name.is_empty() {
        resolve_src_ref(repo, "HEAD")?
    } else {
        match resolve_src_ref(repo, src_name) {
            Ok(oid) => oid,
            Err(_) => match local.count_refname_match(src_name) {
                0 => {
                    return Err(PushPlanError::SrcRefspecNoMatch {
                        spec: src_name.to_owned(),
                    });
                }
                1 => {
                    let Some((name, oid)) = local.refname_match(src_name) else {
                        return Err(PushPlanError::SrcRefspecNoMatch {
                            spec: src_name.to_owned(),
                        });
                    };
                    let dst = resolve_dst_for_src(repo, item, &name, ctx)?;
                    return Ok(vec![push_spec(
                        Some(oid),
                        Some(name.clone()),
                        dst,
                        item.force,
                        false,
                    )]);
                }
                _ => {
                    return Err(PushPlanError::SrcRefspecAmbiguous {
                        spec: src_name.to_owned(),
                    });
                }
            },
        }
    };

    let known_src = if item.exact_sha1
        || src_name.is_empty()
        || src_name.eq_ignore_ascii_case("HEAD")
        || src_name == "@"
    {
        None
    } else {
        local.refname_match(src_name)
    };
    let src_full = if item.exact_sha1 {
        None
    } else if let Some((name, _)) = &known_src {
        Some(name.clone())
    } else {
        Some(full_src_ref_name(repo, src_name, local)?)
    };
    let src_label = src_full.clone();
    let dst = resolve_dst_for_src(repo, item, src_label.as_deref().unwrap_or(src_name), ctx)?;
    let mut spec = push_spec(Some(src_oid), src_label, dst, item.force, false);
    if matches!(src_name, "HEAD" | "@") {
        spec.hook_src_ref = Some("HEAD".to_owned());
    }
    Ok(vec![spec])
}

fn push_spec_from_parsed_item(
    repo: &Repository,
    item: &RefspecItem,
    ctx: &PushPlanContext,
    known_src: (String, ObjectId),
) -> PlanResult<Vec<PushRefSpec>> {
    let src_name = item.src.as_deref().unwrap_or("");
    let src_oid = known_src.1;

    let src_full = if item.exact_sha1 {
        None
    } else {
        Some(known_src.0.clone())
    };
    let src_label = src_full.clone();
    let dst = resolve_dst_for_src(repo, item, src_label.as_deref().unwrap_or(src_name), ctx)?;
    let mut spec = push_spec(Some(src_oid), src_label, dst, item.force, false);
    if matches!(src_name, "HEAD" | "@") {
        spec.hook_src_ref = Some("HEAD".to_owned());
    }
    Ok(vec![spec])
}

fn resolve_dst_for_src(
    repo: &Repository,
    item: &RefspecItem,
    src_full: &str,
    ctx: &PushPlanContext,
) -> PlanResult<String> {
    if let Some(dst) = &item.dst {
        if dst.is_empty() {
            return Err(PushPlanError::InvalidRefspec {
                spec: String::new(),
                reason: "empty destination".into(),
            });
        }
        return resolve_dst_name(repo, dst, Some(src_full), ctx);
    }
    // Missing dst: use same ref name on remote (symref target for branches).
    if src_full.starts_with("refs/heads/") {
        return Ok(src_full.to_owned());
    }
    if src_full.starts_with("refs/tags/") {
        return Ok(src_full.to_owned());
    }
    Ok(src_full.to_owned())
}

fn resolve_dst_name(
    _repo: &Repository,
    dst: &str,
    src_full: Option<&str>,
    ctx: &PushPlanContext,
) -> PlanResult<String> {
    if dst.starts_with("refs/") {
        return Ok(dst.to_owned());
    }
    let remote_list = remote_ref_list(ctx);
    match count_refname_match(dst, &remote_list) {
        0 => {}
        1 => {
            let Some((name, _)) = refname_match(dst, &remote_list) else {
                return Err(PushPlanError::InvalidRefspec {
                    spec: dst.to_owned(),
                    reason: "remote ref match failed".into(),
                });
            };
            return Ok(name);
        }
        _ => {
            return Err(PushPlanError::DstRefspecAmbiguous {
                spec: dst.to_owned(),
            });
        }
    }
    if let Some(src) = src_full {
        if let Some(guessed) = guess_qualified_dst(src, dst) {
            return Ok(guessed);
        }
    }
    Ok(format!("refs/heads/{dst}"))
}

fn remote_ref_list(ctx: &PushPlanContext) -> Vec<(String, ObjectId)> {
    ctx.remote_ref_names
        .iter()
        .filter_map(|n| {
            // oid unused for matching; use zero placeholder.
            ObjectId::from_hex("0".repeat(40).as_str())
                .ok()
                .map(|z| (n.clone(), z))
        })
        .collect()
}

fn guess_qualified_dst(src_full: &str, short_dst: &str) -> Option<String> {
    if src_full.starts_with("refs/heads/") {
        Some(format!("refs/heads/{short_dst}"))
    } else if src_full.starts_with("refs/tags/") {
        Some(format!("refs/tags/{short_dst}"))
    } else {
        None
    }
}

/// Fill `force`, `expected_old`, and `expect_absent` on planned specs from [`ForceMode`].
fn apply_force_mode(
    repo: &Repository,
    remote: &str,
    specs: &mut [PushRefSpec],
    mode: &ForceMode,
) -> PlanResult<()> {
    match mode {
        ForceMode::None => {}
        ForceMode::Force => {
            for spec in specs.iter_mut() {
                spec.force = true;
                spec.expected_old = None;
                spec.expect_absent = false;
            }
        }
        ForceMode::Lease(LeaseExpectation::Tracking) => {
            for spec in specs.iter_mut() {
                apply_tracking_lease(repo, remote, spec);
            }
        }
        ForceMode::Lease(LeaseExpectation::Value(expected)) => {
            if specs.len() != 1 {
                return Err(PushPlanError::LeaseValueRequiresSingleRef);
            }
            for spec in specs.iter_mut() {
                spec.force = true;
                spec.expected_old = Some(*expected);
                spec.expect_absent = false;
            }
        }
    }
    Ok(())
}

/// Let `spec` replace the remote ref when it still holds what this clone's
/// remote-tracking ref for it says (`--force-with-lease` for one ref), or
/// is absent when there is none.
pub fn apply_tracking_lease(repo: &Repository, remote: &str, spec: &mut PushRefSpec) {
    spec.force = true;
    spec.expected_old = None;
    spec.expect_absent = false;
    let Ok(config) = repo.config().snapshot() else {
        return;
    };
    if let Some(tracking) = remote_tracking_ref_for_remote_ref(config.as_ref(), remote, &spec.dst) {
        match refs::resolve_ref(repo.git_dir(), &tracking) {
            Ok(oid) => spec.expected_old = Some(oid),
            Err(_) => spec.expect_absent = true,
        }
    } else {
        spec.expect_absent = true;
    }
}

fn push_spec(
    src: Option<ObjectId>,
    src_ref: Option<String>,
    dst: String,
    force: bool,
    delete: bool,
) -> PushRefSpec {
    PushRefSpec {
        src,
        src_ref,
        dst,
        force,
        delete,
        ..Default::default()
    }
}

fn resolve_src_ref(repo: &Repository, spec: &str) -> PlanResult<ObjectId> {
    resolve_revision_for_verify(repo, spec).map_err(|_| PushPlanError::UnknownRevision {
        spec: spec.to_owned(),
    })
}

fn full_src_ref_name(repo: &Repository, spec: &str, local: &LocalPushRefs) -> PlanResult<String> {
    if spec.eq_ignore_ascii_case("HEAD") {
        if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
            return Ok(format!("refs/heads/{short_name}"));
        }
    }
    if spec.starts_with("refs/") {
        return Ok(spec.to_owned());
    }
    if let Some((name, _)) = local.refname_match(spec) {
        return Ok(name);
    }
    Ok(spec.to_owned())
}

/// Local heads and tags loaded once for refspec planning.
struct LocalPushRefs {
    all: Vec<(String, ObjectId)>,
    by_full: HashMap<String, ObjectId>,
    by_short: HashMap<String, Vec<(String, ObjectId)>>,
    heads: Vec<(String, ObjectId)>,
    tags: Vec<(String, ObjectId)>,
}

impl LocalPushRefs {
    fn load(repo: &Repository) -> PlanResult<Self> {
        let heads = list_local_refs(repo.git_dir(), "refs/heads/")?;
        let tags = list_local_refs(repo.git_dir(), "refs/tags/")?;
        let mut all = Vec::with_capacity(heads.len() + tags.len());
        let mut by_full = HashMap::with_capacity(heads.len() + tags.len());
        let mut by_short: HashMap<String, Vec<(String, ObjectId)>> = HashMap::new();
        for entry in heads.iter().chain(tags.iter()) {
            all.push(entry.clone());
            by_full.insert(entry.0.clone(), entry.1);
            if let Some(short) = entry
                .0
                .strip_prefix("refs/heads/")
                .or_else(|| entry.0.strip_prefix("refs/tags/"))
            {
                by_short
                    .entry(short.to_owned())
                    .or_default()
                    .push(entry.clone());
            }
        }
        Ok(Self {
            all,
            by_full,
            by_short,
            heads,
            tags,
        })
    }

    fn heads(&self) -> &[(String, ObjectId)] {
        &self.heads
    }

    fn tags(&self) -> &[(String, ObjectId)] {
        &self.tags
    }

    fn count_refname_match(&self, pattern: &str) -> usize {
        if (pattern.starts_with("refs/heads/") || pattern.starts_with("refs/tags/"))
            && self.by_full.contains_key(pattern)
        {
            return 1;
        }
        if let Some(entries) = self.by_short.get(pattern) {
            return count_refname_match(pattern, entries);
        }
        count_refname_match(pattern, &self.all)
    }

    fn refname_match(&self, pattern: &str) -> Option<(String, ObjectId)> {
        if pattern.starts_with("refs/heads/") || pattern.starts_with("refs/tags/") {
            if let Some(oid) = self.by_full.get(pattern) {
                return Some((pattern.to_owned(), *oid));
            }
        }
        if let Some(entries) = self.by_short.get(pattern) {
            return refname_match(pattern, entries);
        }
        refname_match(pattern, &self.all)
    }
}

fn list_local_refs(git_dir: &Path, prefix: &str) -> PlanResult<Vec<(String, ObjectId)>> {
    refs::list_refs(git_dir, prefix).map_err(|e| PushPlanError::Nested(Box::new(e)))
}

/// Port of Git `count_refspec_match` / `refname_match` for local refs.
fn count_refname_match(pattern: &str, refs: &[(String, ObjectId)]) -> usize {
    let mut strong = 0usize;
    let mut weak = 0usize;
    let patlen = pattern.len();
    for (name, _) in refs {
        if !refname_match_one(pattern, name) {
            continue;
        }
        let namelen = name.len();
        if namelen != patlen
            && patlen != namelen.saturating_sub(5)
            && !name.starts_with("refs/heads/")
            && !name.starts_with("refs/tags/")
        {
            weak += 1;
        } else {
            strong += 1;
        }
    }
    if strong == 0 {
        weak
    } else {
        strong
    }
}

fn refname_match(pattern: &str, refs: &[(String, ObjectId)]) -> Option<(String, ObjectId)> {
    let mut strong: Option<(String, ObjectId)> = None;
    let mut weak: Option<(String, ObjectId)> = None;
    let patlen = pattern.len();
    for (name, oid) in refs {
        if !refname_match_one(pattern, name) {
            continue;
        }
        let namelen = name.len();
        if namelen != patlen
            && patlen != namelen.saturating_sub(5)
            && !name.starts_with("refs/heads/")
            && !name.starts_with("refs/tags/")
        {
            weak = Some((name.clone(), *oid));
        } else {
            strong = Some((name.clone(), *oid));
        }
    }
    strong.or(weak)
}

fn refname_match_one(pattern: &str, refname: &str) -> bool {
    if pattern == refname {
        return true;
    }
    if refname.ends_with(pattern) {
        // `main` matches `refs/heads/main`
        if refname == format!("refs/heads/{pattern}") || refname == format!("refs/tags/{pattern}") {
            return true;
        }
        if pattern.len() + 5 == refname.len() && refname.starts_with("refs/heads/") {
            return refname.strip_prefix("refs/heads/") == Some(pattern);
        }
        if pattern.len() + 5 == refname.len() && refname.starts_with("refs/tags/") {
            return refname.strip_prefix("refs/tags/") == Some(pattern);
        }
    }
    false
}

fn refspec_map_src(item: &RefspecItem, src_ref: &str) -> Option<String> {
    let src_pat = item.src.as_deref()?;
    let dst_pat = item.dst.as_deref()?;
    match_refname_with_pattern(src_pat, src_ref, dst_pat)
}

/// Whether a planned push ref should be dropped by a configured `^` refspec.
fn push_spec_excluded_by_negative(item: &RefspecItem, spec: &PushRefSpec) -> bool {
    if !item.negative {
        return false;
    }
    let local = spec.src_ref.as_deref().or_else(|| {
        spec.dst
            .starts_with("refs/heads/")
            .then_some(spec.dst.as_str())
    });
    let Some(local) = local else {
        return false;
    };
    negative_excludes_local_ref(item, local)
}

fn match_refname_with_pattern(pattern: &str, refname: &str, replacement: &str) -> Option<String> {
    match (pattern.find('*'), replacement.find('*')) {
        (Some(pstar), Some(vstar)) => {
            let kprefix = &pattern[..pstar];
            let ksuffix = &pattern[pstar + 1..];
            if !refname.starts_with(kprefix) || !refname.ends_with(ksuffix) {
                return None;
            }
            if refname.len() < kprefix.len() + ksuffix.len() {
                return None;
            }
            let middle = &refname[kprefix.len()..refname.len() - ksuffix.len()];
            Some(format!(
                "{}{}{}",
                &replacement[..vstar],
                middle,
                &replacement[vstar + 1..]
            ))
        }
        (None, None) if pattern == refname => Some(replacement.to_owned()),
        _ => None,
    }
}

fn delete_branch_label(specs: &[PushRefSpec]) -> Option<String> {
    if specs.len() != 1 {
        return None;
    }
    specs[0].dst.strip_prefix("refs/heads/").map(str::to_owned)
}

fn branch_label_from_specs(_repo: &Repository, specs: &[PushRefSpec]) -> Option<String> {
    if specs.len() != 1 || specs[0].delete {
        return None;
    }
    let dst = &specs[0].dst;
    dst.strip_prefix("refs/heads/")
        .or_else(|| dst.strip_prefix("refs/tags/"))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::environment::Environment;
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
        (tmp, repo)
    }

    #[test]
    fn push_remote_precedence_pushremote_over_remote() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
[remote "upstream"]
	url = /tmp/u.git
[branch "main"]
	remote = origin
	pushRemote = upstream
"#,
        )
        .expect("write config");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let remote = configured_remote_for_push(&repo, &config, None).expect("remote");
        assert_eq!(remote, "upstream");
    }

    #[test]
    fn map_through_skips_malformed_remote_push_refspec() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
	push = not a valid refspec at all
	push = refs/heads/*:refs/remotes/origin/*
"#,
        )
        .expect("write config");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let mapped = map_through_remote_push_refspec(&config, "origin", "refs/heads/main");
        assert_eq!(
            mapped.as_deref(),
            Some("refs/heads/main:refs/remotes/origin/main")
        );
    }

    #[test]
    fn force_with_lease_tracking_sets_expected_old() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
	fetch = +refs/heads/*:refs/remotes/origin/*
[branch "main"]
	remote = origin
	merge = refs/heads/main
"#,
        )
        .expect("config");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let oid = ObjectId::from_hex("a".repeat(40).as_str()).expect("oid");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads dir");
        std::fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("main");
        std::fs::create_dir_all(git_dir.join("refs/remotes/origin")).expect("rt dir");
        std::fs::write(
            git_dir.join("refs/remotes/origin/main"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("tracking");
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let ctx = PushPlanContext::default();
        let resolved = resolve_push(
            &repo,
            &PushRequest {
                force: ForceMode::Lease(LeaseExpectation::Tracking),
                ..Default::default()
            },
            &ctx,
        )
        .expect("resolve");
        assert_eq!(resolved.specs.len(), 1);
        assert_eq!(resolved.specs[0].expected_old, Some(oid));
        assert!(resolved.specs[0].force);
    }

    #[test]
    fn negative_push_refspec_filters_planned_spec() {
        let item = parse_push_refspec("^refs/heads/private").expect("parse");
        let kept = push_spec(
            Some(ObjectId::from_hex("0".repeat(40).as_str()).expect("oid")),
            Some("refs/heads/main".into()),
            "refs/heads/main".into(),
            false,
            false,
        );
        let dropped = push_spec(
            Some(ObjectId::from_hex("1".repeat(40).as_str()).expect("oid")),
            Some("refs/heads/private".into()),
            "refs/heads/private".into(),
            false,
            false,
        );
        assert!(!push_spec_excluded_by_negative(&item, &kept));
        assert!(push_spec_excluded_by_negative(&item, &dropped));
    }

    fn config_from(text: &str) -> ConfigSet {
        let file = crate::config::ConfigFile::parse(
            Path::new("config"),
            text,
            crate::config::ConfigScope::Local,
        )
        .expect("parse config");
        let mut config = ConfigSet::new();
        config.merge(&file);
        config
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn push_invocation_remote_flag_keeps_every_positional_as_refspec() {
        let config = ConfigSet::new();
        let invocation =
            resolve_push_invocation(&config, Some("upstream"), &strings(&["origin", "main"]))
                .expect("invocation");
        assert_eq!(
            invocation,
            PushInvocation {
                remote: Some("upstream".into()),
                refspecs: strings(&["origin", "main"]),
            }
        );
    }

    #[test]
    fn push_invocation_without_positionals_uses_default_remote() {
        let invocation = resolve_push_invocation(&ConfigSet::new(), None, &[]).expect("invocation");
        assert_eq!(invocation, PushInvocation::default());
    }

    #[test]
    fn push_invocation_first_positional_is_configured_remote() {
        let config = config_from("[remote \"upstream\"]\n\turl = /tmp/u.git\n");
        for positionals in [strings(&["upstream"]), strings(&["upstream", "a", "b:c"])] {
            let invocation =
                resolve_push_invocation(&config, None, &positionals).expect("invocation");
            assert_eq!(invocation.remote.as_deref(), Some("upstream"));
            assert_eq!(invocation.refspecs, positionals[1..]);
        }
    }

    #[test]
    fn push_invocation_first_positional_is_literal_url_or_path() {
        let config = ConfigSet::new();
        for remote in [
            "https://example.com/r",
            "../sibling.git",
            "/srv/repo",
            "../srv",
            "./srv",
            "./../srv",
            "../deep/nb3",
            ".",
            "..",
        ] {
            let invocation = resolve_push_invocation(&config, None, &strings(&[remote, "main"]))
                .expect("invocation");
            assert_eq!(invocation.remote.as_deref(), Some(remote));
            assert_eq!(invocation.refspecs, strings(&["main"]));
        }
    }

    #[test]
    fn push_invocation_single_unknown_positional_is_a_refspec() {
        let config = config_from("[remote \"blank\"]\n\turl =   \n");
        for token in ["main", "HEAD:refs/heads/x", "blank"] {
            let invocation =
                resolve_push_invocation(&config, None, &strings(&[token])).expect("invocation");
            assert_eq!(invocation.remote, None, "{token}");
            assert_eq!(invocation.refspecs, strings(&[token]));
        }
    }

    #[test]
    fn push_without_any_remote_is_no_remote() {
        let (_tmp, repo) = init_repo();
        assert!(matches!(
            configured_remote_for_push(&repo, &ConfigSet::new(), None),
            Err(PushPlanError::NoRemote)
        ));
        assert_eq!(PushPlanError::NoRemote.to_string(), "no remote to push to");
        assert!(matches!(
            load_push_plan_context(&repo, None, None).map(|_| ()),
            Err(PushPlanError::NoRemote)
        ));
        let named = configured_remote_for_push(&repo, &ConfigSet::new(), Some("upstream"));
        assert_eq!(named.as_deref().unwrap(), "upstream");
        let url_less_origin = config_from("[remote \"origin\"]\n\tpushurl = /tmp/o.git\n");
        assert_eq!(
            configured_remote_for_push(&repo, &url_less_origin, None)
                .as_deref()
                .unwrap(),
            "origin"
        );
    }

    #[test]
    fn push_invocation_unknown_remote_before_refspecs_is_rejected() {
        let err = resolve_push_invocation(&ConfigSet::new(), None, &strings(&["nosuch", "main"]))
            .expect_err("unknown remote");
        assert!(matches!(err, PushPlanError::UnknownRemote(name) if name == "nosuch"));
    }

    #[test]
    fn owner_project_shorthand_expands_to_host_url() {
        let config = config_from("[remote \"team/app\"]\n\turl = /srv/app.git\n");
        let expand = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            expand_repository_shorthand(&config, &args, "https://grithub.dev/")
        };
        assert_eq!(
            expand(&["me/proj", "main"]).as_deref(),
            Some("https://grithub.dev/me/proj.git")
        );
        // A lone positional is a refspec, not a repository.
        assert_eq!(expand(&["topic/x"]), None);
        // Configured remotes, URLs and paths are left alone.
        for literal in [
            "team/app",
            "origin",
            "./a/b",
            "a/b/c",
            "x/y.git",
            "https://h/a/b",
        ] {
            assert_eq!(expand(&[literal, "main"]), None, "{literal}");
        }
    }

    #[test]
    fn open_push_destination_reads_config_or_treats_literal_as_url() {
        let config = config_from(
            "[remote \"origin\"]\n\turl = /tmp/o.git\n[remote \"nourl\"]\n\tfetch = +refs/heads/*:refs/remotes/nourl/*\n",
        );
        let origin = open_push_destination(&config, "origin").expect("origin");
        assert_eq!(origin.name.as_deref(), Some("origin"));
        for path in ["/srv/other.git", "../srv", "./srv"] {
            let literal = open_push_destination(&config, path).expect("literal");
            assert_eq!(literal.name, None, "{path}");
        }
        assert!(matches!(
            open_push_destination(&config, "nourl"),
            Err(RemoteError::NoUrl { remote: Some(name) }) if name == "nourl"
        ));
    }

    #[test]
    fn plan_context_from_advertisement_skips_peeled_entries() {
        let oid = |c: &str| ObjectId::from_hex(&c.repeat(40)).expect("oid");
        let remote_ref = |name: &str, hex: &str| RemoteRef {
            name: name.to_owned(),
            oid: oid(hex),
            symref_target: None,
        };
        let ctx = PushPlanContext::from_advertised_refs([
            remote_ref("HEAD", "1"),
            remote_ref("refs/heads/main", "1"),
            remote_ref("refs/heads/topic", "2"),
            remote_ref("refs/tags/v1", "3"),
            remote_ref("refs/tags/v1^{}", "4"),
        ]);
        let mut names: Vec<_> = ctx.remote_ref_names.iter().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "HEAD",
                "refs/heads/main",
                "refs/heads/topic",
                "refs/tags/v1"
            ]
        );
        assert_eq!(ctx.remote_branch_tips, HashSet::from([oid("1"), oid("2")]));
        assert_eq!(
            ctx.remote_tips,
            Some(vec![oid("1"), oid("1"), oid("2"), oid("3")])
        );
    }

    #[test]
    fn plan_context_from_empty_advertisement_knows_remote_has_no_tips() {
        let ctx = PushPlanContext::from_advertised_refs([]);
        assert_eq!(ctx.remote_tips, Some(Vec::new()));
    }

    #[test]
    fn plan_context_is_empty_for_unconfigured_remote() {
        let (_tmp, repo) = init_repo();
        let ctx = load_push_plan_context(&repo, Some("nosuch"), None).expect("context");
        assert!(ctx.remote_ref_names.is_empty());
        assert!(ctx.remote_branch_tips.is_empty());
        assert_eq!(ctx.remote_tips, None);
    }

    #[test]
    fn plan_context_for_missing_path_assumes_no_remote_objects() {
        let (tmp, repo) = init_repo();
        let missing = tmp.path().join("missing.git");
        let missing = missing.to_str().expect("utf-8 path");
        let ctx = load_push_plan_context(&repo, Some(missing), None).expect("context");
        assert!(ctx.remote_ref_names.is_empty());
        assert!(ctx.remote_tips.as_ref().is_none_or(Vec::is_empty));
    }

    #[test]
    fn tag_push_branch_label_is_tag_name_not_current_branch() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
"#,
        )
        .expect("config");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let oid = ObjectId::from_hex("a".repeat(40).as_str()).expect("oid");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads");
        std::fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("main");
        std::fs::create_dir_all(git_dir.join("refs/tags")).expect("tags");
        std::fs::write(git_dir.join("refs/tags/v1"), format!("{}\n", oid.to_hex())).expect("tag");
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let ctx = PushPlanContext::default();
        let resolved = resolve_push(
            &repo,
            &PushRequest {
                remote: Some("origin".to_owned()),
                refspecs: vec!["v1".to_owned()],
                ..Default::default()
            },
            &ctx,
        )
        .expect("resolve tag push");
        assert_eq!(resolved.branch_label.as_deref(), Some("v1"));
        assert_eq!(resolved.kind, PushPlanKind::Refspecs);
        assert_eq!(resolved.specs[0].dst, "refs/tags/v1");
    }

    fn delete_request(refspecs: &[&str]) -> PushRequest {
        PushRequest {
            remote: Some("origin".to_owned()),
            refspecs: strings(refspecs),
            delete_refs: true,
            ..Default::default()
        }
    }

    #[test]
    fn delete_push_plans_every_plain_name() {
        let (_tmp, repo) = init_repo();
        let config = config_from("[remote \"origin\"]\n\turl = /tmp/o.git\n");
        let ctx = PushPlanContext {
            remote_ref_names: HashSet::from(["refs/tags/v1".to_owned()]),
            ..Default::default()
        };
        let resolved = resolve_push(
            &repo,
            &delete_request(&["a", "refs/heads/b", "v1", "+c", "HEAD"]),
            &ctx,
        )
        .expect("resolve delete push");
        assert_eq!(resolved.kind, PushPlanKind::Delete);
        assert_eq!(resolved.branch_label, None);
        let dsts: Vec<_> = resolved.specs.iter().map(|s| s.dst.as_str()).collect();
        assert_eq!(
            dsts,
            [
                "refs/heads/a",
                "refs/heads/b",
                "refs/tags/v1",
                "refs/heads/+c",
                "refs/heads/HEAD"
            ]
        );
        assert!(resolved
            .specs
            .iter()
            .all(|s| s.delete && !s.force && s.src.is_none()));
    }

    #[test]
    fn delete_push_unions_delete_list_with_refspecs() {
        let (_tmp, repo) = init_repo();
        let config = config_from("[remote \"origin\"]\n\turl = /tmp/o.git\n");
        let request = PushRequest {
            delete: strings(&["a"]),
            ..delete_request(&["b"])
        };
        let resolved = resolve_push(&repo, &request, &PushPlanContext::default())
            .expect("resolve delete push");
        let dsts: Vec<_> = resolved.specs.iter().map(|s| s.dst.as_str()).collect();
        assert_eq!(dsts, ["refs/heads/a", "refs/heads/b"]);
        assert_eq!(
            resolve_push(
                &repo,
                &PushRequest {
                    delete_refs: false,
                    ..request
                },
                &PushPlanContext::default(),
            )
            .unwrap()
            .specs
            .len(),
            2
        );
    }

    #[test]
    fn delete_push_rejects_colon_refspec() {
        let (_tmp, repo) = init_repo();
        let config = config_from("[remote \"origin\"]\n\turl = /tmp/o.git\n");
        for spec in ["HEAD:refs/heads/release", ":b", "a:"] {
            let err = resolve_push(
                &repo,
                &delete_request(&["ok", spec]),
                &PushPlanContext::default(),
            )
            .expect_err("colon refspec with --delete");
            assert!(
                matches!(
                    &err,
                    PushPlanError::DeleteNotPlainName { spec: s } if s == spec
                ),
                "{err}"
            );
            assert!(err.to_string().contains(spec), "{err}");
        }
    }

    #[test]
    fn delete_push_without_refs_is_an_error() {
        let (_tmp, repo) = init_repo();
        let err = resolve_push(&repo, &delete_request(&[]), &PushPlanContext::default())
            .expect_err("empty delete");
        assert!(matches!(err, PushPlanError::DeleteWithoutRefs));
    }

    #[test]
    fn many_shorthand_refspecs_resolve_local_heads() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
"#,
        )
        .expect("config");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let oid = ObjectId::from_hex("a".repeat(40).as_str()).expect("oid");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads");
        std::fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("main");
        for i in 1..=500 {
            std::fs::write(
                git_dir.join(format!("refs/heads/b{i}")),
                format!("{}\n", oid.to_hex()),
            )
            .expect("branch");
        }
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let refspecs: Vec<String> = (1..=500).map(|i| format!("b{i}")).collect();
        let resolved = resolve_push(
            &repo,
            &PushRequest {
                remote: Some("origin".to_owned()),
                refspecs,
                ..Default::default()
            },
            &PushPlanContext::default(),
        )
        .expect("resolve many shorthands");
        assert_eq!(resolved.specs.len(), 500);
        for (i, spec) in resolved.specs.iter().enumerate() {
            let n = i + 1;
            assert_eq!(spec.dst, format!("refs/heads/b{n}"));
            assert_eq!(spec.src, Some(oid));
            assert_eq!(
                spec.src_ref.as_deref(),
                Some(format!("refs/heads/b{n}").as_str())
            );
        }
    }

    #[test]
    fn shorthand_ambiguous_head_and_tag_same_name() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
"#,
        )
        .expect("config");
        let oid = ObjectId::from_hex("b".repeat(40).as_str()).expect("oid");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads");
        std::fs::create_dir_all(git_dir.join("refs/tags")).expect("tags");
        std::fs::write(
            git_dir.join("refs/heads/release"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("head");
        std::fs::write(
            git_dir.join("refs/tags/release"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("tag");
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let err = resolve_push(
            &repo,
            &PushRequest {
                remote: Some("origin".to_owned()),
                refspecs: vec!["release".to_owned()],
                ..Default::default()
            },
            &PushPlanContext::default(),
        )
        .expect_err("ambiguous");
        assert!(matches!(
            err,
            PushPlanError::SrcRefspecAmbiguous { spec } if spec == "release"
        ));
    }

    #[test]
    fn shorthand_honors_remote_push_mapping() {
        let (_tmp, repo) = init_repo();
        let git_dir = repo.git_dir();
        std::fs::write(
            git_dir.join("config"),
            r#"[remote "origin"]
	url = /tmp/o.git
	push = refs/heads/main:refs/remotes/origin/main
"#,
        )
        .expect("config");
        let oid = ObjectId::from_hex("c".repeat(40).as_str()).expect("oid");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("heads");
        std::fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("main");
        let config = ConfigSet::load(
            &Environment::empty(),
            Some(git_dir),
            &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(git_dir)),
        )
        .expect("config");
        let resolved = resolve_push(
            &repo,
            &PushRequest {
                remote: Some("origin".to_owned()),
                refspecs: vec!["main".to_owned()],
                ..Default::default()
            },
            &PushPlanContext::default(),
        )
        .expect("mapped shorthand");
        assert_eq!(resolved.specs.len(), 1);
        assert_eq!(resolved.specs[0].dst, "refs/remotes/origin/main");
        assert_eq!(resolved.specs[0].src, Some(oid));
    }
}
