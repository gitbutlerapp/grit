//! Resolve remotes and refspecs for [`Remote::push`](crate::remote::Remote::push).
//!
//! [`resolve_push`] turns a [`PushRequest`] (CLI flags and positional refspecs)
//! into a [`ResolvedPush`] of concrete [`PushRefSpec`] entries, matching Git's
//! push refspec and remote-selection rules.

use std::collections::HashSet;
use std::path::Path;

use thiserror::Error;

use crate::branch_tracking::remote_tracking_ref_for_remote_ref;
use crate::config::ConfigSet;
use crate::objects::ObjectId;
use crate::refs;
use crate::refspec::{parse_push_refspec, RefspecItem};
use crate::repo::Repository;
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
    /// Remote refs to delete (`--delete`).
    pub delete: Vec<String>,
    /// Force or force-with-lease mode from CLI flags.
    pub force: ForceMode,
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
}

/// Errors while planning a push (typed; no stringly matching in callers).
#[derive(Debug, Error, PartialEq, Eq)]
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
    /// Other planning failure.
    #[error("{0}")]
    Message(String),
}

type PlanResult<T> = std::result::Result<T, PushPlanError>;

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
pub fn resolve_push(
    repo: &Repository,
    config: &ConfigSet,
    request: &PushRequest,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    if !request.delete.is_empty() {
        return resolve_delete_push(repo, config, request, ctx);
    }
    if request.tags {
        return resolve_tags_push(repo, config, request);
    }

    let remote = configured_remote_for_push(repo, config, request.remote.as_deref())?;
    validate_remote(config, &remote)?;

    if request.refspecs.is_empty() {
        let mut resolved = resolve_default_push(repo, config, &remote, ctx)?;
        apply_force_mode(repo, config, &remote, &mut resolved.specs, &request.force)?;
        return Ok(resolved);
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
    let branch_label = branch_label_from_specs(repo, &specs);
    let mut resolved = ResolvedPush {
        remote,
        specs,
        branch_label,
        kind: PushPlanKind::Refspecs,
    };
    apply_force_mode(
        repo,
        config,
        &resolved.remote,
        &mut resolved.specs,
        &request.force,
    )?;
    Ok(resolved)
}

fn resolve_delete_push(
    repo: &Repository,
    config: &ConfigSet,
    request: &PushRequest,
    ctx: &PushPlanContext,
) -> PlanResult<ResolvedPush> {
    let remote = configured_remote_for_push(repo, config, request.remote.as_deref())?;
    validate_remote(config, &remote)?;
    let specs = request
        .delete
        .iter()
        .map(|name| {
            let dst = resolve_push_delete_ref(repo.git_dir(), &ctx.remote_ref_names, name);
            Ok(push_spec(None, None, dst, false, true))
        })
        .collect::<PlanResult<Vec<_>>>()?;
    let branch_label = delete_branch_label(&specs);
    let mut resolved = ResolvedPush {
        remote,
        specs,
        branch_label,
        kind: PushPlanKind::Delete,
    };
    apply_force_mode(
        repo,
        config,
        &resolved.remote,
        &mut resolved.specs,
        &request.force,
    )?;
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
        .map_err(|e| PushPlanError::Message(e.to_string()))?;
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
    apply_force_mode(
        repo,
        config,
        &resolved.remote,
        &mut resolved.specs,
        &request.force,
    )?;
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
        let mut specs = Vec::new();
        for spec in positive_specs {
            specs.extend(expand_configured_push_refspec(repo, spec, ctx)?);
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
        .get("push.default")
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
    match resolve_head(repo.git_dir()).map_err(|e| PushPlanError::Message(e.to_string()))? {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => Ok((short_name, oid)),
        HeadState::Branch { .. } => Err(PushPlanError::NoCommits),
        HeadState::Detached { .. } => Err(PushPlanError::DetachedHeadNoRefspec),
        HeadState::Invalid => Err(PushPlanError::Message("HEAD is in an unknown state".into())),
    }
}

/// Remote selection for push: explicit, `branch.*.pushRemote`, `remote.pushDefault`,
/// `branch.*.remote`, then `origin`.
pub fn configured_remote_for_push(
    repo: &Repository,
    config: &ConfigSet,
    remote_override: Option<&str>,
) -> PlanResult<String> {
    if let Some(remote) = remote_override.filter(|r| !r.trim().is_empty()) {
        return Ok(remote.to_owned());
    }
    if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
        if let Some(remote) = config
            .get(&format!("branch.{short_name}.pushremote"))
            .or_else(|| config.get(&format!("branch.{short_name}.pushRemote")))
            .filter(|r| !r.trim().is_empty())
        {
            return Ok(remote);
        }
    }
    if let Some(remote) = config
        .get("remote.pushdefault")
        .or_else(|| config.get("remote.pushDefault"))
        .filter(|r| !r.trim().is_empty())
    {
        return Ok(remote);
    }
    if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
        if let Some(remote) = config
            .get(&format!("branch.{short_name}.remote"))
            .filter(|r| !r.trim().is_empty() && r != ".")
        {
            return Ok(remote);
        }
    }
    Ok(DEFAULT_REMOTE.to_owned())
}

fn looks_like_literal_remote(name: &str) -> bool {
    name.contains("://") || name.ends_with(".git") || name.starts_with('/')
}

fn validate_remote(config: &ConfigSet, name: &str) -> PlanResult<()> {
    if config
        .get(&format!("remote.{name}.url"))
        .filter(|u| !u.trim().is_empty())
        .is_some()
    {
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
        .get_all(&format!("remote.{remote}.push"))
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
        .get("push.default")
        .unwrap_or_else(|| "simple".to_owned());

    if push_default == "nothing" {
        return Err(PushPlanError::PushDefaultNothing);
    }

    if push_default.as_str() == "upstream" {
        let merge = config
            .get(&format!("branch.{branch_short}.merge"))
            .filter(|m| m.starts_with("refs/"));
        return merge.ok_or_else(|| {
            PushPlanError::Message(format!(
                "branch '{branch_short}' has no upstream for push.default upstream"
            ))
        });
    }

    // `simple`, `current`, and unspecified: grit keeps the historical default of
    // `branch.<name>.merge` when set, else same branch name on the remote.
    if push_default.as_str() == "simple" {
        let branch_remote = config.get(&format!("branch.{branch_short}.remote"));
        if branch_remote.as_deref() == Some(remote) {
            if let Some(merge) = config
                .get(&format!("branch.{branch_short}.merge"))
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
) -> PlanResult<Vec<PushRefSpec>> {
    if !spec.contains(':') {
        return resolve_shorthand_refspec(repo, config, remote, spec, ctx);
    }
    let item = parse_push_refspec(spec).map_err(|e| PushPlanError::InvalidRefspec {
        spec: spec.to_owned(),
        reason: e.to_string(),
    })?;
    resolve_parsed_refspec(repo, &item, ctx)
}

fn resolve_shorthand_refspec(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    spec: &str,
    ctx: &PushPlanContext,
) -> PlanResult<Vec<PushRefSpec>> {
    let local_heads = list_local_refs(repo.git_dir(), "refs/heads/")?;
    let local_tags = list_local_refs(repo.git_dir(), "refs/tags/")?;
    let mut all = local_heads;
    all.extend(local_tags);

    let count = count_refname_match(spec, &all);
    if count == 1 {
        let Some(matched) = refname_match(spec, &all) else {
            return Err(PushPlanError::SrcRefspecNoMatch {
                spec: spec.to_owned(),
            });
        };
        if let Some(mapped) = map_through_remote_push_refspec(config, remote, &matched.0) {
            let item = parse_push_refspec(&mapped).map_err(|e| PushPlanError::InvalidRefspec {
                spec: mapped.clone(),
                reason: e.to_string(),
            })?;
            return resolve_parsed_refspec(repo, &item, ctx);
        }
        let item = RefspecItem {
            force: false,
            src: Some(matched.0.clone()),
            dst: Some(matched.0.clone()),
            ..Default::default()
        };
        return resolve_parsed_refspec(repo, &item, ctx);
    }
    if count == 0 {
        // Treat as explicit refspec string (Git refspec_append).
        let item = parse_push_refspec(spec).map_err(|e| PushPlanError::InvalidRefspec {
            spec: spec.to_owned(),
            reason: e.to_string(),
        })?;
        return resolve_parsed_refspec(repo, &item, ctx);
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
) -> PlanResult<Vec<PushRefSpec>> {
    let item = parse_push_refspec(&spec).map_err(|e| PushPlanError::InvalidRefspec {
        spec: spec.clone(),
        reason: e.to_string(),
    })?;
    if item.matching {
        let heads = list_local_refs(repo.git_dir(), "refs/heads/")?;
        let mut out = Vec::new();
        for (name, oid) in heads {
            if !ctx.remote_ref_names.contains(&name) {
                continue;
            }
            out.push(push_spec(
                Some(oid),
                Some(name.clone()),
                name,
                item.force,
                false,
            ));
        }
        return Ok(out);
    }
    if item.pattern {
        let prefix = item.src.as_deref().unwrap_or("");
        let local = if prefix.starts_with("refs/tags/") {
            list_local_refs(repo.git_dir(), "refs/tags/")?
        } else {
            list_local_refs(repo.git_dir(), "refs/heads/")?
        };
        let mut out = Vec::new();
        for (name, _) in local {
            if let Some(dst) = refspec_map_src(&item, &name) {
                let oid = resolve_src_ref(repo, &name)?;
                out.push(push_spec(
                    Some(oid),
                    Some(name.clone()),
                    dst,
                    item.force,
                    false,
                ));
            }
        }
        return Ok(out);
    }
    resolve_parsed_refspec(repo, &item, ctx)
}

fn resolve_parsed_refspec(
    repo: &Repository,
    item: &RefspecItem,
    ctx: &PushPlanContext,
) -> PlanResult<Vec<PushRefSpec>> {
    if item.negative {
        return Ok(Vec::new());
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
            Err(_) => {
                let local = list_local_refs(repo.git_dir(), "refs/heads/")?;
                let local_tags = list_local_refs(repo.git_dir(), "refs/tags/")?;
                let mut all = local;
                all.extend(local_tags);
                match count_refname_match(src_name, &all) {
                    0 => {
                        return Err(PushPlanError::SrcRefspecNoMatch {
                            spec: src_name.to_owned(),
                        });
                    }
                    1 => {
                        let Some((name, oid)) = refname_match(src_name, &all) else {
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
                }
            }
        }
    };

    let src_full = if item.exact_sha1 {
        None
    } else {
        Some(full_src_ref_name(repo, src_name)?)
    };
    let src_label = src_full.clone();
    let dst = resolve_dst_for_src(repo, item, src_label.as_deref().unwrap_or(src_name), ctx)?;
    Ok(vec![push_spec(
        Some(src_oid),
        src_label,
        dst,
        item.force,
        false,
    )])
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
    config: &ConfigSet,
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
                apply_tracking_lease(repo, config, remote, spec);
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

fn apply_tracking_lease(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    spec: &mut PushRefSpec,
) {
    spec.force = true;
    spec.expected_old = None;
    spec.expect_absent = false;
    if let Some(tracking) = remote_tracking_ref_for_remote_ref(config, remote, &spec.dst) {
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

fn full_src_ref_name(repo: &Repository, spec: &str) -> PlanResult<String> {
    if spec.eq_ignore_ascii_case("HEAD") {
        if let Ok(HeadState::Branch { short_name, .. }) = resolve_head(repo.git_dir()) {
            return Ok(format!("refs/heads/{short_name}"));
        }
    }
    if spec.starts_with("refs/") {
        return Ok(spec.to_owned());
    }
    let heads = list_local_refs(repo.git_dir(), "refs/heads/")?;
    if let Some((name, _)) = refname_match(spec, &heads) {
        return Ok(name);
    }
    let tags = list_local_refs(repo.git_dir(), "refs/tags/")?;
    if let Some((name, _)) = refname_match(spec, &tags) {
        return Ok(name);
    }
    Ok(spec.to_owned())
}

fn list_local_refs(git_dir: &Path, prefix: &str) -> PlanResult<Vec<(String, ObjectId)>> {
    refs::list_refs(git_dir, prefix).map_err(|e| PushPlanError::Message(e.to_string()))
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
    let Some(pat) = item.src.as_deref() else {
        return false;
    };
    let local = spec.src_ref.as_deref().or_else(|| {
        spec.dst
            .starts_with("refs/heads/")
            .then_some(spec.dst.as_str())
    });
    let Some(local) = local else {
        return false;
    };
    if item.pattern {
        return match_refname_with_pattern(pat, local, pat).is_some();
    }
    pat == local || refname_match_one(pat, local)
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

fn branch_label_from_specs(repo: &Repository, specs: &[PushRefSpec]) -> Option<String> {
    if specs.len() != 1 || specs[0].delete {
        return None;
    }
    let dst = &specs[0].dst;
    dst.strip_prefix("refs/heads/")
        .map(str::to_owned)
        .or_else(|| {
            resolve_head(repo.git_dir()).ok().and_then(|h| match h {
                HeadState::Branch { short_name, .. } => Some(short_name),
                _ => None,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::environment::Environment;
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
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
        let config = ConfigSet::load(&Environment::empty(), Some(git_dir), true).expect("config");
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
        let config = ConfigSet::load(&Environment::empty(), Some(git_dir), true).expect("config");
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
        let config = ConfigSet::load(&Environment::empty(), Some(git_dir), true).expect("config");
        let ctx = PushPlanContext::default();
        let resolved = resolve_push(
            &repo,
            &config,
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
}
