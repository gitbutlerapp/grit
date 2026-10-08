//! Revision parsing and repository discovery helpers for `rev-parse`.
//!
//! This module implements a focused subset of Git's revision parser used by
//! `grit rev-parse` in v2 scope: repository/work-tree discovery flags, basic
//! object-name resolution, and lightweight peeling (`^{}`, `^{object}`,
//! `^{commit}`).

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

use regex::Regex;

use std::collections::{HashMap, HashSet};

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::pack;
use crate::reflog::read_reflog;
use crate::refs;
use crate::repo::Repository;
use crate::rev_parse_error::{AmbiguousObjectHint, RevParseError};

/// Return `Some(repo)` when a repository can be discovered at `start`.
///
/// # Parameters
///
/// - `start` - starting path for discovery; when `None`, uses current directory.
///
/// # Errors
///
/// Returns errors other than "not a repository" (for example I/O and path
/// canonicalization failures).
pub fn discover_optional(start: Option<&Path>) -> Result<Option<Repository>> {
    match Repository::discover(start) {
        Ok(repo) => Ok(Some(repo)),
        Err(Error::NotARepository(msg)) => {
            // Repository not found while walking parents is optional, but
            // structural `.git` problems at the starting directory should be
            // surfaced so callers can show diagnostics (e.g. t0002/t0009).
            if msg.contains("invalid gitfile format")
                || msg.contains("gitfile does not contain 'gitdir:' line")
                || msg.contains("not a regular file")
            {
                return Err(Error::NotARepository(msg));
            }

            if let Some(start) = start {
                let start = if start.is_absolute() {
                    start.to_path_buf()
                } else if let Ok(cwd) = std::env::current_dir() {
                    cwd.join(start)
                } else {
                    start.to_path_buf()
                };
                let dot_git = start.join(".git");
                if dot_git.is_file() || dot_git.is_symlink() {
                    return Err(Error::NotARepository(msg));
                }
            }

            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// Compute whether `cwd` is inside the repository's work tree.
#[must_use]
pub fn is_inside_work_tree(repo: &Repository, cwd: &Path) -> bool {
    let Some(work_tree) = &repo.work_tree else {
        return false;
    };
    path_is_within(cwd, work_tree)
}

/// Compute whether `cwd` is inside the repository's git-dir.
#[must_use]
pub fn is_inside_git_dir(repo: &Repository, cwd: &Path) -> bool {
    path_is_within(cwd, &repo.git_dir)
}

/// Compute the `--show-prefix` output.
///
/// Returns an empty string when `cwd` is at repository root or outside the work
/// tree. Returned prefixes always use `/` separators and end with `/`.
#[must_use]
pub fn show_prefix(repo: &Repository, cwd: &Path) -> String {
    let Some(work_tree) = &repo.work_tree else {
        return String::new();
    };
    if !path_is_within(cwd, work_tree) {
        return String::new();
    }
    if cwd == work_tree {
        return String::new();
    }
    let Ok(rel) = cwd.strip_prefix(work_tree) else {
        return String::new();
    };
    let mut out = rel
        .components()
        .filter_map(component_to_text)
        .collect::<Vec<_>>()
        .join("/");
    if !out.is_empty() {
        out.push('/');
    }
    out
}

/// Superproject work tree when `git_dir` lives under `.../<wt>/.git/modules/...` (nested submodule).
///
/// Used when the submodule's recorded path in the superproject index does not match the on-disk
/// layout (e.g. `dir/sub` recorded but git dir is `.../modules/dir/modules/sub`), so
/// `ls-files`-based superproject detection cannot find a gitlink.
#[must_use]
pub fn superproject_work_tree_from_nested_git_modules(git_dir: &Path) -> Option<PathBuf> {
    let mut p = git_dir.to_path_buf();
    while let Some(parent) = p.parent() {
        if p.file_name().is_some_and(|n| n == "modules")
            && parent.file_name().is_some_and(|n| n == ".git")
        {
            return parent.parent().map(PathBuf::from);
        }
        if parent == p {
            break;
        }
        p = parent.to_path_buf();
    }
    None
}

/// Resolve a symbolic ref name to its full form.
///
/// For `HEAD`, returns the symbolic target (e.g., `refs/heads/main`).
/// For branch names, returns `refs/heads/<name>`.
/// For tag names, returns `refs/tags/<name>`.
/// Returns `None` when the name cannot be resolved symbolically.
#[must_use]
pub fn symbolic_full_name(repo: &Repository, spec: &str) -> Option<String> {
    // @{upstream} / @{push}: must error from rev-parse when invalid; do not fall through to DWIM.
    if upstream_suffix_info(spec).is_some() {
        return resolve_upstream_symbolic_name(repo, spec).ok();
    }

    if let Ok(Some(branch)) = expand_at_minus_to_branch_name(repo, spec) {
        let ref_name = format!("refs/heads/{branch}");
        if refs::resolve_ref(&repo.git_dir, &ref_name).is_ok() {
            return Some(ref_name);
        }
        return None;
    }

    if spec == "HEAD" {
        if let Ok(Some(target)) = refs::read_symbolic_ref(&repo.git_dir, "HEAD") {
            return Some(target);
        }
        return None;
    }
    // If it's already a full ref path
    if spec.starts_with("refs/") {
        if refs::resolve_ref(&repo.git_dir, spec).is_ok() {
            return Some(spec.to_owned());
        }
        return None;
    }
    // DWIM: try refs/heads, refs/tags, refs/remotes
    for prefix in &["refs/heads/", "refs/tags/", "refs/remotes/"] {
        let candidate = format!("{prefix}{spec}");
        if refs::resolve_ref(&repo.git_dir, &candidate).is_ok() {
            return Some(candidate);
        }
    }
    // Remote name alone: `one` → `refs/remotes/one/HEAD` when `remote.one.url` exists (matches Git).
    if let Some(full) = remote_tracking_head_symbolic_target(repo, spec) {
        return Some(full);
    }
    None
}

/// When `name` is a configured remote, return the full ref `refs/remotes/<name>/HEAD` resolves to.
fn remote_tracking_head_symbolic_target(repo: &Repository, name: &str) -> Option<String> {
    if name.contains('/')
        || matches!(
            name,
            "HEAD" | "FETCH_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
        )
    {
        return None;
    }
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )
    .ok()?;
    let url_key = format!("remote.{name}.url");
    config.get(&url_key)?;
    let head_ref = format!("refs/remotes/{name}/HEAD");
    let target = refs::read_symbolic_ref(&repo.git_dir, &head_ref).ok()??;
    Some(target)
}

/// Expand an `@{-N}` token to the corresponding previous branch name.
///
/// Returns:
/// - `Ok(Some(branch_name))` when `spec` is an `@{-N}` token and resolves
///   to a branch name.
/// - `Ok(None)` when `spec` is not an `@{-N}` token.
/// - `Err(...)` when `spec` matches `@{-N}` syntax but cannot be resolved.
pub fn expand_at_minus_to_branch_name(repo: &Repository, spec: &str) -> Result<Option<String>> {
    if !spec.starts_with("@{-") || !spec.ends_with('}') {
        return Ok(None);
    }
    let inner = &spec[3..spec.len() - 1];
    let n: usize = inner
        .parse()
        .map_err(|_| Error::InvalidRef(format!("invalid N in @{{-N}} for '{spec}'")))?;
    if n < 1 {
        return Ok(None);
    }
    resolve_at_minus_to_branch(repo, n).map(Some)
}

/// Resolve `@{-N}` to the commit OID it points to.
pub fn resolve_at_minus_to_oid(repo: &Repository, spec: &str) -> Result<Option<ObjectId>> {
    try_resolve_at_minus(repo, spec)
}

/// Abbreviate a full ref name to its shortest unambiguous form.
///
/// For example, `refs/heads/main` becomes `main`.
#[must_use]
pub fn abbreviate_ref_name(full_name: &str) -> String {
    for prefix in &["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(short) = full_name.strip_prefix(prefix) {
            return short.to_owned();
        }
    }
    if let Some(short) = full_name.strip_prefix("refs/") {
        return short.to_owned();
    }
    full_name.to_owned()
}

/// Returns `(base_without_suffix, is_push)` when `spec` ends with `@{upstream}` / `@{u}` / `@{push}`
/// (case-insensitive for upstream forms). `is_push` is true only for `@{push}`.
#[must_use]
pub fn upstream_suffix_info(spec: &str) -> Option<(&str, bool)> {
    let lower = spec.to_ascii_lowercase();
    if lower.ends_with("@{push}") {
        let base = &spec[..spec.len() - 7];
        return Some((base, true));
    }
    if lower.ends_with("@{upstream}") {
        let base = &spec[..spec.len() - 11];
        return Some((base, false));
    }
    if lower.ends_with("@{u}") {
        let base = &spec[..spec.len() - 4];
        return Some((base, false));
    }
    None
}

/// Resolve `@{upstream}` / `@{u}` / `@{push}` to the symbolic full ref name (for `rev-parse --symbolic-full-name`).
pub fn resolve_upstream_symbolic_name(repo: &Repository, spec: &str) -> Result<String> {
    let Some((base, is_push)) = upstream_suffix_info(spec) else {
        return Err(Error::InvalidRef(format!("not an upstream spec: {spec}")));
    };
    resolve_upstream_full_ref_name(repo, base, is_push)
}

fn resolve_upstream_full_ref_name(repo: &Repository, base: &str, is_push: bool) -> Result<String> {
    if is_push {
        return resolve_push_ref_name(repo, base);
    }
    let (branch_key, display_branch) = resolve_upstream_branch_context(repo, base)?;
    let config_path = repo.git_dir.join("config");
    let config_content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let Some((remote, merge)) = parse_branch_tracking(&config_content, &branch_key) else {
        return Err(RevParseError::NoUpstream {
            branch: display_branch,
        }
        .into());
    };
    if remote == "." {
        let m = merge.trim();
        if m.starts_with("refs/") {
            return Ok(m.to_owned());
        }
        return Ok(format!("refs/heads/{m}"));
    }
    let merge_branch = merge
        .strip_prefix("refs/heads/")
        .ok_or_else(|| Error::InvalidRef(format!("invalid merge ref: {merge}")))?;
    let tracking = format!("refs/remotes/{remote}/{merge_branch}");
    if refs::resolve_ref(&repo.git_dir, &tracking).is_err() {
        return Err(RevParseError::UpstreamNotTracked {
            merge: merge.to_owned(),
        }
        .into());
    }
    Ok(tracking)
}

/// Resolve the remote-tracking ref used as `@{push}` for `branch_short` (`refs/heads/...` name).
///
/// Honors `remote.pushRemote`, `branch.<name>.pushRemote`, `push.default`, and per-remote
/// `push` refspecs (exact `refs/heads/<branch>:refs/heads/<dest>` mappings).
pub fn resolve_push_full_ref_for_branch(repo: &Repository, branch_short: &str) -> Result<String> {
    let config_path = crate::refs::common_dir(&repo.git_dir)
        .unwrap_or_else(|| repo.git_dir.clone())
        .join("config");
    let config_content = fs::read_to_string(&config_path).map_err(Error::Io)?;

    let upstream_tracking =
        parse_branch_tracking(&config_content, branch_short).and_then(|(remote, merge)| {
            if remote == "." {
                return None;
            }
            let mb = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
            let tr = format!("refs/remotes/{remote}/{mb}");
            if refs::resolve_ref(&repo.git_dir, &tr).is_ok() {
                Some(tr)
            } else {
                None
            }
        });

    let push_remote = parse_config_value(&config_content, "remote", "pushRemote")
        .or_else(|| parse_config_value(&config_content, "remote", "pushDefault"))
        .or_else(|| {
            let section = format!("[branch \"{}\"]", branch_short);
            let mut in_section = false;
            for line in config_content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('[') {
                    in_section = trimmed == section;
                    continue;
                }
                if in_section {
                    if let Some(v) = trimmed
                        .strip_prefix("pushremote = ")
                        .or_else(|| trimmed.strip_prefix("pushRemote = "))
                    {
                        return Some(v.trim().to_owned());
                    }
                }
            }
            None
        })
        .or_else(|| {
            parse_branch_tracking(&config_content, branch_short)
                .map(|(r, _)| r)
                .filter(|r| r != ".")
        });

    let Some(push_remote_name) = push_remote else {
        return upstream_tracking.ok_or_else(|| RevParseError::NoPushRemote.into());
    };

    // When the push remote has any configured `push` refspecs, they take priority
    // over `push.default` entirely (matching git's branch_get_push_1: the
    // `if (remote->push.nr)` block returns before the push.default switch). This
    // means an explicit push refspec overrides even `push.default = nothing`.
    if remote_has_push_refspec(&config_content, &push_remote_name) {
        return match push_refspec_mapped_tracking(&config_content, &push_remote_name, branch_short)
        {
            Some(mapped) => Ok(mapped),
            None => Err(RevParseError::PushRefspecMissing {
                remote: push_remote_name,
                branch: branch_short.to_owned(),
            }
            .into()),
        };
    }

    let push_default = parse_config_value(&config_content, "push", "default");
    let push_default = push_default.as_deref().unwrap_or("simple");

    if push_default == "nothing" {
        return Err(RevParseError::PushDefaultNothing.into());
    }

    let current_tracking = format!("refs/remotes/{push_remote_name}/{branch_short}");

    match push_default {
        "upstream" => upstream_tracking.ok_or_else(|| {
            RevParseError::NoUpstreamForPush {
                branch: branch_short.to_owned(),
            }
            .into()
        }),
        "simple" => {
            if let Some(ref up) = upstream_tracking {
                if up == &current_tracking
                    && refs::resolve_ref(&repo.git_dir, &current_tracking).is_ok()
                {
                    return Ok(current_tracking);
                }
            }
            Err(RevParseError::PushDefaultSimpleMismatch.into())
        }
        _ => {
            if refs::resolve_ref(&repo.git_dir, &current_tracking).is_ok() {
                Ok(current_tracking)
            } else if let Some(up) = upstream_tracking {
                Ok(up)
            } else {
                Err(RevParseError::NoPushTrackingRef {
                    branch: branch_short.to_owned(),
                }
                .into())
            }
        }
    }
}

/// Collect the values of all `push = <refspec>` entries in `[remote "<name>"]`.
fn remote_push_refspecs(config_content: &str, remote_name: &str) -> Vec<String> {
    let section = format!("[remote \"{remote_name}\"]");
    let mut in_section = false;
    let mut out = Vec::new();
    for line in config_content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == section;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(val) = trimmed
            .strip_prefix("push = ")
            .or_else(|| trimmed.strip_prefix("push="))
        {
            if let Some(spec) = val.split_whitespace().next() {
                out.push(spec.trim().to_owned());
            }
        }
    }
    out
}

/// True when the remote has at least one `push` refspec configured.
fn remote_has_push_refspec(config_content: &str, remote_name: &str) -> bool {
    !remote_push_refspecs(config_content, remote_name).is_empty()
}

/// Match `refname` against a refspec `pattern` (`prefix*suffix`), substituting the
/// matched glob portion into `replacement` (also `prefix*suffix`). Mirrors git's
/// `match_refname_with_pattern`. For non-pattern refspecs, both sides are exact.
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
        (None, None) => {
            if pattern == refname {
                Some(replacement.to_owned())
            } else {
                None
            }
        }
        // A pattern on one side but not the other is invalid in git; treat as no match.
        _ => None,
    }
}

/// Apply the remote's configured `push` refspecs to `refs/heads/<branch_short>`, then
/// map the resulting push destination to a local remote-tracking ref via the remote's
/// `fetch` refspecs. Mirrors git's `apply_refspecs(&remote->push, ...)` followed by
/// `tracking_for_push_dest` (which applies `&remote->fetch`).
fn push_refspec_mapped_tracking(
    config_content: &str,
    remote_name: &str,
    branch_short: &str,
) -> Option<String> {
    let src = format!("refs/heads/{branch_short}");

    // First, map the branch through a push refspec to the push destination on the remote.
    let mut dst = None;
    for spec in remote_push_refspecs(config_content, remote_name) {
        let spec = spec.strip_prefix('+').unwrap_or(&spec);
        let Some((left, right)) = spec.split_once(':') else {
            continue;
        };
        if let Some(mapped) = match_refname_with_pattern(left.trim(), &src, right.trim()) {
            dst = Some(mapped);
            break;
        }
    }
    let dst = dst?;

    // Then map the push destination to a local tracking ref via the fetch refspecs.
    map_dest_to_tracking(config_content, remote_name, &dst)
}

/// Map a push destination ref (e.g. `refs/heads/magic/topic`) on `remote_name` to the
/// local remote-tracking ref using that remote's `fetch` refspecs. Falls back to the
/// conventional `refs/remotes/<remote>/<rest>` mapping when no fetch refspec matches.
fn map_dest_to_tracking(config_content: &str, remote_name: &str, dst: &str) -> Option<String> {
    let section = format!("[remote \"{remote_name}\"]");
    let mut in_section = false;
    for line in config_content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == section;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(val) = trimmed
            .strip_prefix("fetch = ")
            .or_else(|| trimmed.strip_prefix("fetch="))
        else {
            continue;
        };
        let Some(spec) = val.split_whitespace().next() else {
            continue;
        };
        let spec = spec.trim().strip_prefix('+').unwrap_or(spec.trim());
        let Some((left, right)) = spec.split_once(':') else {
            continue;
        };
        if let Some(mapped) = match_refname_with_pattern(left.trim(), dst, right.trim()) {
            return Some(mapped);
        }
    }
    // Conventional fallback: refs/heads/<x> -> refs/remotes/<remote>/<x>.
    dst.strip_prefix("refs/heads/")
        .map(|rest| format!("refs/remotes/{remote_name}/{rest}"))
}

fn resolve_push_ref_name(repo: &Repository, base: &str) -> Result<String> {
    let (branch_key, _display) = resolve_upstream_branch_context(repo, base)?;
    resolve_push_full_ref_for_branch(repo, &branch_key)
}

/// Returns `(config_branch_key, display_name_for_errors)` for upstream resolution.
fn resolve_upstream_branch_context(repo: &Repository, base: &str) -> Result<(String, String)> {
    let base = if base == "HEAD" {
        Cow::Borrowed("")
    } else if base.starts_with("@{-") && base.ends_with('}') {
        if let Ok(Some(b)) = expand_at_minus_to_branch_name(repo, base) {
            Cow::Owned(b)
        } else {
            Cow::Borrowed(base)
        }
    } else {
        Cow::Borrowed(base)
    };
    let base = base.as_ref();
    let base = if base == "@" { "" } else { base };

    if base.is_empty() {
        let Some(head) = refs::read_head(&repo.git_dir)? else {
            return Err(RevParseError::HeadNotBranch.into());
        };
        let Some(short) = head.strip_prefix("refs/heads/") else {
            return Err(RevParseError::HeadNotBranch.into());
        };
        return Ok((short.to_owned(), short.to_owned()));
    }
    let head_branch = refs::read_head(&repo.git_dir)?.and_then(|h| {
        h.strip_prefix("refs/heads/")
            .map(std::borrow::ToOwned::to_owned)
    });
    if head_branch.as_deref() == Some(base) {
        return Ok((base.to_owned(), base.to_owned()));
    }
    let refname = format!("refs/heads/{base}");
    if refs::resolve_ref(&repo.git_dir, &refname).is_err() {
        return Err(RevParseError::NoSuchBranch {
            branch: base.to_owned(),
        }
        .into());
    }
    Ok((base.to_owned(), base.to_owned()))
}

fn parse_config_value(config: &str, section: &str, key: &str) -> Option<String> {
    let section_header = format!("[{}]", section);
    let key_lower = key.to_ascii_lowercase();
    let mut in_section = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed.eq_ignore_ascii_case(&section_header);
            continue;
        }
        if in_section {
            let lower = trimmed.to_ascii_lowercase();
            if lower.starts_with(&key_lower) {
                let rest = lower[key_lower.len()..].trim_start().to_string();
                if rest.starts_with('=') {
                    if let Some(eq_pos) = trimmed.find('=') {
                        return Some(trimmed[eq_pos + 1..].trim().to_owned());
                    }
                }
            }
        }
    }
    None
}

/// Parse branch tracking configuration from git config content.
fn parse_branch_tracking(config: &str, branch: &str) -> Option<(String, String)> {
    let mut remote = None;
    let mut merge = None;
    let mut in_section = false;
    let target_section = format!("[branch \"{}\"]", branch);

    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == target_section
                || trimmed.starts_with(&format!("[branch \"{}\"", branch));
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("remote = ") {
            remote = Some(value.trim().to_owned());
        } else if let Some(value) = trimmed.strip_prefix("merge = ") {
            merge = Some(value.trim().to_owned());
        }
        // Also handle with tabs
        if let Some(value) = trimmed.strip_prefix("remote=") {
            remote = Some(value.trim().to_owned());
        } else if let Some(value) = trimmed.strip_prefix("merge=") {
            merge = Some(value.trim().to_owned());
        }
    }

    match (remote, merge) {
        (Some(r), Some(m)) => Some((r, m)),
        _ => None,
    }
}

/// Resolve a revision string to an object ID.
///
/// Supports:
/// - full 40-hex object IDs (must exist in loose store),
/// - abbreviated object IDs (length 4-39, must resolve uniquely),
/// - direct refs (`HEAD`, `refs/...`),
/// - DWIM branch/tag/remote names (`name` -> `refs/heads/name`, etc.),
/// - peeling suffixes: `^{}`, `^{object}`, `^{commit}`.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] or [`Error::InvalidRef`] when resolution
/// fails.
/// Split `spec` at a `..` range operator, avoiding the three-dot symmetric-diff form.
///
/// Returns `(left, right)` where either side may be empty (`..HEAD`, `HEAD..`, `..`).
#[must_use]
/// Load commit parent overrides from `.git/info/grafts` (same format as Git).
///
/// Used for `^N` / `^@` / `^!` / `^-` resolution and for `rev-list` traversal.
pub fn load_graft_parents(git_dir: &Path) -> HashMap<ObjectId, Vec<ObjectId>> {
    let graft_path = crate::repo::common_git_dir_for_config(git_dir).join("info/grafts");
    let mut grafts = HashMap::new();
    let Ok(contents) = fs::read_to_string(&graft_path) else {
        return grafts;
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(commit_hex) = fields.next() else {
            continue;
        };
        let Ok(commit_oid) = commit_hex.parse::<ObjectId>() else {
            continue;
        };
        let mut parents = Vec::new();
        let mut valid = true;
        for parent_hex in fields {
            match parent_hex.parse::<ObjectId>() {
                Ok(parent_oid) => parents.push(parent_oid),
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            grafts.insert(commit_oid, parents);
        }
    }
    grafts
}

/// Parent OIDs of `commit_oid` for revision navigation, honoring grafts.
pub fn commit_parents_for_navigation(
    repo: &Repository,
    commit_oid: ObjectId,
) -> Result<Vec<ObjectId>> {
    let obj = repo.odb.read(&commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::InvalidRef(format!(
            "invalid ref: {commit_oid} is not a commit"
        )));
    }
    let commit = parse_commit(&obj.data)?;
    let mut parents = commit.parents;
    let grafts = load_graft_parents(&repo.git_dir);
    if let Some(grafted) = grafts.get(&commit_oid) {
        parents = grafted.clone();
    }
    Ok(parents)
}

#[derive(Debug, Clone, Copy)]
enum ParentShorthandKind {
    /// `rev^@` — all parents.
    At,
    /// `rev^!` — include `rev`, exclude all parents (merge-safe).
    Bang,
    /// `rev^-` / `rev^-N` — include `rev`, exclude parent N (1-based), include other parents.
    Minus { exclude_parent: usize },
}

/// Returns true when `spec` ends with Git parent shorthands `^@`, `^!`, or `^-` / `^-N`.
#[must_use]
pub fn spec_has_parent_shorthand_suffix(spec: &str) -> bool {
    find_parent_shorthand(spec).is_some()
}

fn find_parent_shorthand(spec: &str) -> Option<(usize, ParentShorthandKind)> {
    let mut best: Option<(usize, ParentShorthandKind, u8)> = None;
    for (idx, _) in spec.match_indices('^') {
        let Some(tail) = spec.get(idx + 1..) else {
            continue;
        };
        if tail.starts_with('@') && idx + 2 == spec.len() {
            best = Some((idx, ParentShorthandKind::At, 0));
            break;
        }
        if tail.starts_with('!') && idx + 2 == spec.len() {
            let cand = (idx, ParentShorthandKind::Bang, 1);
            best = Some(match best {
                Some(b) if b.2 < 1 => b,
                _ => cand,
            });
            continue;
        }
        if let Some(after) = tail.strip_prefix('-') {
            let (exclude_parent, valid) = if after.is_empty() {
                (1usize, true)
            } else if after.bytes().all(|b| b.is_ascii_digit()) && !after.is_empty() {
                let n: usize = after.parse().unwrap_or(0);
                (n, n >= 1)
            } else {
                (0, false)
            };
            if !valid {
                continue;
            }
            let cand = (idx, ParentShorthandKind::Minus { exclude_parent }, 2);
            best = Some(match best {
                Some(b) if b.2 < 2 => b,
                _ => cand,
            });
        }
    }
    best.map(|(i, k, _)| (i, k))
}

/// Expand Git parent shorthands (`^@`, `^!`, `^-`, `^-N`) to the strings `git rev-parse` would print.
///
/// Returns [`None`] when `spec` does not use these suffixes at the end (or the suffix is invalid).
///
/// # Errors
///
/// Returns resolution errors when the base committish cannot be resolved or is not a commit.
pub fn expand_parent_shorthand_rev_parse_lines(
    repo: &Repository,
    spec: &str,
    symbolic: bool,
    short_len: Option<usize>,
) -> Result<Option<Vec<String>>> {
    let Some((mark_idx, kind)) = find_parent_shorthand(spec) else {
        return Ok(None);
    };
    let base_spec = &spec[..mark_idx];
    let base_for_resolve = if base_spec.is_empty() {
        "HEAD"
    } else {
        base_spec
    };
    // Git `--symbolic` prints parent specs using the same spelling as the user would type
    // (e.g. `final^1^1`), not full ref names (`refs/heads/...`).
    let symbolic_base = if base_spec.is_empty() {
        "HEAD"
    } else {
        base_spec
    };
    let tip_oid = resolve_revision_for_range_end(repo, base_for_resolve)?;
    let commit_oid = peel_to_commit_for_merge_base(repo, tip_oid)?;
    let parents = commit_parents_for_navigation(repo, commit_oid)?;

    let mut out = Vec::new();
    match kind {
        ParentShorthandKind::At => {
            if parents.is_empty() {
                return Ok(Some(out));
            }
            for (i, p) in parents.iter().enumerate() {
                let parent_n = i + 1;
                if symbolic {
                    out.push(format!("{symbolic_base}^{parent_n}"));
                } else if let Some(len) = short_len {
                    out.push(abbreviate_object_id(repo, *p, len)?);
                } else {
                    out.push(p.to_string());
                }
            }
        }
        ParentShorthandKind::Bang => {
            if parents.is_empty() {
                if symbolic {
                    out.push(symbolic_base.to_string());
                } else if let Some(len) = short_len {
                    out.push(abbreviate_object_id(repo, commit_oid, len)?);
                } else {
                    out.push(commit_oid.to_string());
                }
                return Ok(Some(out));
            }
            if symbolic {
                out.push(symbolic_base.to_string());
                for (i, _) in parents.iter().enumerate() {
                    let parent_n = i + 1;
                    out.push(format!("^{symbolic_base}^{parent_n}"));
                }
            } else if let Some(len) = short_len {
                out.push(abbreviate_object_id(repo, commit_oid, len)?);
                for p in &parents {
                    out.push(format!("^{}", abbreviate_object_id(repo, *p, len)?));
                }
            } else {
                out.push(commit_oid.to_string());
                for p in &parents {
                    out.push(format!("^{p}"));
                }
            }
        }
        ParentShorthandKind::Minus { exclude_parent } => {
            if exclude_parent > parents.len() {
                return Ok(None);
            }
            let excluded_parent = parents[exclude_parent - 1];
            if symbolic {
                out.push(symbolic_base.to_string());
                out.push(format!("^{symbolic_base}^{exclude_parent}"));
            } else if let Some(len) = short_len {
                out.push(abbreviate_object_id(repo, commit_oid, len)?);
                out.push(format!(
                    "^{}",
                    abbreviate_object_id(repo, excluded_parent, len)?
                ));
            } else {
                out.push(commit_oid.to_string());
                out.push(format!("^{excluded_parent}"));
            }
        }
    }
    Ok(Some(out))
}

pub fn split_double_dot_range(spec: &str) -> Option<(&str, &str)> {
    if spec == ".." {
        return Some(("", ""));
    }
    let bytes = spec.as_bytes();
    let mut search = 0usize;
    while let Some(rel) = spec[search..].find("..") {
        let idx = search + rel;
        // Reject `..` that is part of `...` (symmetric-diff operator).
        let touches_dot_before = idx > 0 && bytes[idx - 1] == b'.';
        let touches_dot_after = idx + 2 < bytes.len() && bytes[idx + 2] == b'.';
        if touches_dot_before || touches_dot_after {
            search = idx + 1;
            continue;
        }
        // Reject `..` that starts a path segment (`../` in `HEAD:../file`).
        if idx + 2 < bytes.len() && (bytes[idx + 2] == b'/' || bytes[idx + 2] == b'\\') {
            search = idx + 1;
            continue;
        }
        let left = &spec[..idx];
        let right = &spec[idx + 2..];
        return Some((left, right));
    }
    None
}

/// Split `spec` at the first `...` symmetric-diff operator (not part of `....`).
///
/// Returns `(left, right)` where either side may be empty (`...HEAD`, `A...`, `...`).
#[must_use]
pub fn split_triple_dot_range(spec: &str) -> Option<(&str, &str)> {
    if spec == "..." {
        return Some(("", ""));
    }
    let bytes = spec.as_bytes();
    let mut search = 0usize;
    while let Some(rel) = spec[search..].find("...") {
        let idx = search + rel;
        let four_before = idx >= 1 && bytes[idx - 1] == b'.';
        let four_after = idx + 3 < bytes.len() && bytes[idx + 3] == b'.';
        if four_before || four_after {
            search = idx + 1;
            continue;
        }
        let left = &spec[..idx];
        let right = &spec[idx + 3..];
        return Some((left, right));
    }
    None
}

/// Like [`resolve_revision`], but does not treat a bare filename as an index path
/// (matches `git rev-parse` / plumbing, where `file.txt` stays ambiguous).
pub fn resolve_revision_without_index_dwim(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, false, false, true, false, false, false, false)
}

/// Resolve a revision string to an object ID.
pub fn resolve_revision(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, true, false, true, false, false, false, true)
}

/// Like [`resolve_revision`], but can disable remote-tracking DWIM used by `git checkout`
/// when `--no-guess` / `checkout.guess=false` (t2024).
pub fn resolve_revision_for_checkout_guess(
    repo: &Repository,
    spec: &str,
    remote_branch_guess: bool,
) -> Result<ObjectId> {
    resolve_revision_impl(
        repo,
        spec,
        true,
        false,
        true,
        false,
        false,
        false,
        remote_branch_guess,
    )
}

/// Resolve `spec` when it appears as the end of a revision range (`A..B`, `A...B`, etc.):
/// abbreviated hex and `core.disambiguate` prefer a commit (porcelain range parsing).
pub fn resolve_revision_for_range_end(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, true, true, true, false, false, false, true)
}

/// Like [`resolve_revision_for_range_end`], but does not resolve a bare filename as an index path.
///
/// Matches plumbing-style revision parsing (`git rev-parse` without index DWIM). Used when a
/// token must not be confused with a tracked path that happens to match a branch name (e.g.
/// `git reset --hard` after `submodule update` when the submodule has a branch `sub1` and the
/// superproject index lists path `sub1`).
pub fn resolve_revision_for_range_end_without_index_dwim(
    repo: &Repository,
    spec: &str,
) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, false, true, true, false, false, false, true)
}

/// Resolve a single revision for `git rev-parse --verify` (no index path DWIM).
///
/// Git's `--verify` mode must reject tokens that only match an index entry when the path is
/// missing from the work tree (`t7102-reset` disambiguation).
pub fn resolve_revision_for_verify(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, false, true, true, false, false, false, true)
}

/// First argument to `commit-tree`: ambiguous short hex uses tree-ish rules (blob vs tree).
pub fn resolve_revision_for_commit_tree_tree(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, true, false, true, false, true, false, true)
}

/// Old blob OID from a patch `index <old>..<new>` line (`git apply --build-fake-ancestor`).
pub fn resolve_revision_for_patch_old_blob(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision_impl(repo, spec, true, false, true, false, false, true, true)
}

/// When `spec` uses two-dot range syntax (`A..B`, `..B`, `A..`), returns the commits to
/// **exclude** (left tip) and **include** (right tip) for `git log`-style walks.
///
/// Returns `Ok(None)` when `spec` is not a two-dot range. Symmetric `A...B` is handled by
/// [`resolve_revision_as_commit`] instead.
///
/// # Errors
///
/// Propagates resolution errors from either range endpoint.
pub fn try_parse_double_dot_log_range(
    repo: &Repository,
    spec: &str,
) -> Result<Option<(ObjectId, ObjectId)>> {
    let Some((left, right)) = split_double_dot_range(spec) else {
        return Ok(None);
    };
    let left_tip = if left.is_empty() {
        resolve_revision_for_range_end(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end(repo, left)?
    };
    let right_tip = if right.is_empty() {
        resolve_revision_for_range_end(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end(repo, right)?
    };
    let left_c = peel_to_commit_for_merge_base(repo, left_tip)?;
    let right_c = peel_to_commit_for_merge_base(repo, right_tip)?;
    Ok(Some((left_c, right_c)))
}

fn try_parse_double_dot_log_range_without_index_dwim(
    repo: &Repository,
    spec: &str,
) -> Result<Option<(ObjectId, ObjectId)>> {
    let Some((left, right)) = split_double_dot_range(spec) else {
        return Ok(None);
    };
    let left_tip = if left.is_empty() {
        resolve_revision_for_range_end_without_index_dwim(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end_without_index_dwim(repo, left)?
    };
    let right_tip = if right.is_empty() {
        resolve_revision_for_range_end_without_index_dwim(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end_without_index_dwim(repo, right)?
    };
    let left_c = peel_to_commit_for_merge_base(repo, left_tip)?;
    let right_c = peel_to_commit_for_merge_base(repo, right_tip)?;
    Ok(Some((left_c, right_c)))
}

/// Resolve `spec` to a commit OID for porcelain history commands (`log`, `reset`, etc.).
///
/// Handles `A..B` / `..B` / `A..` (tip is the right side, defaulting to `HEAD`) and
/// `A...B` symmetric diff (returns the merge base). Other specs are resolved and peeled
/// to a commit (tags peeled, abbreviated hex disambiguated as commit-ish on range ends).
/// Returns true when `spec` ends with Git parent/ancestor navigation (`~N`, `^N`, bare `~`/`^`).
///
/// Used by porcelain (`reset`) to distinguish commit-ish arguments from pathspecs when
/// full resolution is deferred or fails for other reasons.
#[must_use]
pub fn revision_spec_contains_ancestry_navigation(spec: &str) -> bool {
    let (_, steps) = parse_nav_steps(spec);
    !steps.is_empty()
}

pub fn resolve_revision_as_commit(repo: &Repository, spec: &str) -> Result<ObjectId> {
    if let Some((left, right)) = split_triple_dot_range(spec) {
        let left_tip = if left.is_empty() {
            resolve_revision_for_range_end(repo, "HEAD")?
        } else {
            resolve_revision_for_range_end(repo, left)?
        };
        let right_tip = if right.is_empty() {
            resolve_revision_for_range_end(repo, "HEAD")?
        } else {
            resolve_revision_for_range_end(repo, right)?
        };
        let left_c = peel_to_commit_for_merge_base(repo, left_tip)?;
        let right_c = peel_to_commit_for_merge_base(repo, right_tip)?;
        let bases = crate::merge_base::merge_bases_first_vs_rest(repo, left_c, &[right_c])?;
        return bases
            .into_iter()
            .next()
            .ok_or_else(|| Error::ObjectNotFound(format!("no merge base for '{spec}'")));
    }
    if let Some((_excl, tip)) = try_parse_double_dot_log_range(repo, spec)? {
        return Ok(tip);
    }
    let oid = resolve_revision_for_range_end(repo, spec)?;
    peel_to_commit_for_merge_base(repo, oid)
}

/// Like [`resolve_revision_as_commit`], but never treats a bare path as an index revision.
///
/// Use when distinguishing the first `git reset` argument from pathspecs: a submodule work tree
/// may have a branch whose name equals a path recorded in the **superproject** index (t3426).
pub fn resolve_revision_as_commit_without_index_dwim(
    repo: &Repository,
    spec: &str,
) -> Result<ObjectId> {
    if let Some((left, right)) = split_triple_dot_range(spec) {
        let left_tip = if left.is_empty() {
            resolve_revision_for_range_end_without_index_dwim(repo, "HEAD")?
        } else {
            resolve_revision_for_range_end_without_index_dwim(repo, left)?
        };
        let right_tip = if right.is_empty() {
            resolve_revision_for_range_end_without_index_dwim(repo, "HEAD")?
        } else {
            resolve_revision_for_range_end_without_index_dwim(repo, right)?
        };
        let left_c = peel_to_commit_for_merge_base(repo, left_tip)?;
        let right_c = peel_to_commit_for_merge_base(repo, right_tip)?;
        let bases = crate::merge_base::merge_bases_first_vs_rest(repo, left_c, &[right_c])?;
        return bases
            .into_iter()
            .next()
            .ok_or_else(|| Error::ObjectNotFound(format!("no merge base for '{spec}'")));
    }
    if let Some((_excl, tip)) = try_parse_double_dot_log_range_without_index_dwim(repo, spec)? {
        return Ok(tip);
    }
    let oid = resolve_revision_for_range_end_without_index_dwim(repo, spec)?;
    peel_to_commit_for_merge_base(repo, oid)
}

fn resolve_ref_dwim_for_rev_parse(repo: &Repository, spec: &str) -> (usize, Option<ObjectId>) {
    const RULES: &[&str] = &[
        "{0}",
        "refs/{0}",
        "refs/tags/{0}",
        "refs/heads/{0}",
        "refs/remotes/{0}",
        "refs/remotes/{0}/HEAD",
    ];

    let mut count = 0usize;
    let mut first = None;
    let refname_opts = RefNameOptions::default();
    for rule in RULES {
        let candidate = rule.replace("{0}", spec);
        if let Ok(Some(target)) = refs::read_symbolic_ref(&repo.git_dir, &candidate) {
            if check_refname_format(&target, &refname_opts).is_err()
                || refs::resolve_ref(&repo.git_dir, &target).is_err()
            {
                // Match upstream `expand_ref` (refs.c): a dangling symref is
                // silently ignored when it is literally `HEAD` (e.g. an unborn
                // branch in a fresh repo). The warning is only emitted for other
                // dangling symrefs encountered while DWIM-resolving a ref.
                if candidate != "HEAD" {
                    eprintln!("warning: ignoring dangling symref {candidate}");
                }
                continue;
            }
        }
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &candidate) {
            count += 1;
            if first.is_none() {
                first = Some(oid);
            }
        }
    }
    (count, first)
}

#[expect(clippy::too_many_arguments)]
fn resolve_revision_impl(
    repo: &Repository,
    spec: &str,
    index_dwim: bool,
    commit_only_hex: bool,
    use_disambiguate_config: bool,
    treeish_colon_lhs: bool,
    implicit_tree_abbrev: bool,
    implicit_blob_abbrev: bool,
    remote_branch_name_guess: bool,
) -> Result<ObjectId> {
    // Handle `:/message` early — it can contain any characters so must
    // not be confused with peel/nav syntax.
    if let Some(pattern) = spec.strip_prefix(":/") {
        if pattern.is_empty() {
            // `:/` with an empty pattern resolves to the youngest reachable commit (HEAD tip).
            let head = crate::state::resolve_head(&repo.git_dir)
                .map_err(|_| Error::ObjectNotFound(":/".to_owned()))?;
            return head
                .oid()
                .copied()
                .ok_or_else(|| Error::ObjectNotFound(":/".to_owned()));
        }
        return resolve_commit_message_search(repo, pattern);
    }

    if let Some(index_spec) = parse_index_colon_spec(spec) {
        let path = normalize_colon_path_for_tree(repo, index_spec.raw_path)?;
        return resolve_index_path_at_stage(repo, &path, index_spec.stage)
            .map_err(|e| diagnose_index_path_error(repo, &path, index_spec.stage, e));
    }

    // `tags/<name>` is Git's DWIM for `refs/tags/<name>` (t6101 `tags/start`).
    if let Some(tag_path) = spec.strip_prefix("tags/") {
        if !tag_path.is_empty() {
            let tag_ref = format!("refs/tags/{tag_path}");
            if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &tag_ref) {
                return Ok(oid);
            }
        }
    }

    // Pseudo-ref written by `git merge` / grit merge on conflict (tree OID, one line).
    if spec == "AUTO_MERGE" {
        let raw = fs::read_to_string(repo.git_dir.join("AUTO_MERGE")).map_err(|e| {
            Error::from(RevParseError::AutoMergeRead {
                detail: e.to_string(),
            })
        })?;
        let line = raw.lines().next().unwrap_or("").trim();
        return line
            .parse::<ObjectId>()
            .map_err(|_| Error::InvalidRef("AUTO_MERGE: invalid object id".to_owned()));
    }

    // `refs/...` spelled in full (e.g. `refs/tags/other`): resolve as a ref before any
    // treeish / DWIM path logic so a worktree path named `other` cannot shadow `refs/tags/other`
    // (`git rev-parse refs/tags/other`, t5332).
    if spec.starts_with("refs/") && !spec.contains(':') {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, spec) {
            return Ok(oid);
        }
    }

    // Handle A...B (symmetric difference / merge-base)
    // Also handles A... (implies A...HEAD)
    if let Some(idx) = spec.find("...") {
        let left_raw = &spec[..idx];
        let right_raw = &spec[idx + 3..];
        if !left_raw.is_empty() || !right_raw.is_empty() {
            let left_oid = peel_to_commit_for_merge_base(
                repo,
                if left_raw.is_empty() {
                    resolve_revision_impl(
                        repo,
                        "HEAD",
                        index_dwim,
                        commit_only_hex,
                        use_disambiguate_config,
                        false,
                        false,
                        false,
                        remote_branch_name_guess,
                    )?
                } else {
                    resolve_revision_impl(
                        repo,
                        left_raw,
                        index_dwim,
                        commit_only_hex,
                        use_disambiguate_config,
                        false,
                        false,
                        false,
                        remote_branch_name_guess,
                    )?
                },
            )?;
            let right_oid = peel_to_commit_for_merge_base(
                repo,
                if right_raw.is_empty() {
                    resolve_revision_impl(
                        repo,
                        "HEAD",
                        index_dwim,
                        commit_only_hex,
                        use_disambiguate_config,
                        false,
                        false,
                        false,
                        remote_branch_name_guess,
                    )?
                } else {
                    resolve_revision_impl(
                        repo,
                        right_raw,
                        index_dwim,
                        commit_only_hex,
                        use_disambiguate_config,
                        false,
                        false,
                        false,
                        remote_branch_name_guess,
                    )?
                },
            )?;
            let bases = crate::merge_base::merge_bases_first_vs_rest(repo, left_oid, &[right_oid])?;
            return bases
                .into_iter()
                .next()
                .ok_or_else(|| Error::ObjectNotFound(format!("no merge base for '{spec}'")));
        }
    }

    // Handle <rev>:<path> — resolve a tree entry.
    // Must come after :/ handling. The colon must not be inside `^{...}` (e.g.
    // `other^{/msg:}:file`) and must not be the `:path` / `:N:path` index forms.
    if let Some((before, after)) = split_treeish_colon(spec) {
        if !before.is_empty() && !spec.starts_with(":/") {
            // <rev>:<path> — resolve rev to tree, then navigate path
            let rev_oid = match resolve_revision_impl(
                repo,
                before,
                index_dwim,
                commit_only_hex,
                use_disambiguate_config,
                true,
                false,
                false,
                remote_branch_name_guess,
            ) {
                Ok(o) => o,
                Err(Error::ObjectNotFound(s)) if s == before => {
                    return Err(RevParseError::InvalidObjectName {
                        name: before.to_owned(),
                    }
                    .into());
                }
                Err(Error::RevParse(RevParseError::AmbiguousArgument { .. })) => {
                    return Err(RevParseError::InvalidObjectName {
                        name: before.to_owned(),
                    }
                    .into());
                }
                Err(e) => return Err(e),
            };
            let tree_oid = peel_to_tree(repo, rev_oid)?;
            if after.is_empty() {
                // <rev>: means the tree itself
                return Ok(tree_oid);
            }
            let clean_path = match normalize_colon_path_for_tree(repo, after) {
                Ok(p) => p,
                Err(Error::InvalidRef(msg)) if msg == "outside repository" => {
                    let wt = repo
                        .work_tree
                        .as_ref()
                        .and_then(|p| p.canonicalize().ok())
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    return Err(RevParseError::OutsideRepository {
                        path: after.to_owned(),
                        work_tree: wt,
                    }
                    .into());
                }
                Err(e) => return Err(e),
            };
            return resolve_tree_path_rev_parse(repo, &tree_oid, &clean_path)
                .map_err(|e| diagnose_tree_path_error(repo, before, after, &clean_path, e));
        }
    }

    let (base_with_nav, peel) = parse_peel_suffix(spec);
    let (base, nav_steps) = parse_nav_steps(base_with_nav);
    let peel_for_hex = peel
        .or(((treeish_colon_lhs || implicit_tree_abbrev) && peel.is_none()).then_some("tree"))
        .or((implicit_blob_abbrev && peel.is_none()).then_some("blob"));
    let mut oid = resolve_base(
        repo,
        base,
        index_dwim,
        commit_only_hex,
        use_disambiguate_config,
        peel_for_hex,
        implicit_tree_abbrev,
        implicit_blob_abbrev,
        remote_branch_name_guess,
    )?;
    for step in nav_steps {
        oid = apply_nav_step(repo, oid, step).map_err(|e| {
            if matches!(e, Error::ObjectNotFound(_)) {
                RevParseError::AmbiguousArgument {
                    spec: spec.to_owned(),
                }
                .into()
            } else {
                e
            }
        })?;
    }
    apply_peel(repo, oid, peel)
}

/// Normalize a path from `treeish:path` against the work tree and return a `/`-separated path
/// relative to the repository root (for tree lookup).
fn normalize_path_components(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(x) => out.push(x),
        }
    }
    out
}

/// Normalize `treeish:path` path segment for tree lookup when there is no work tree (bare repo).
///
/// Paths are interpreted relative to the repository root; `./` / `../` / `.` still require a work
/// tree in Git and are rejected here.
fn normalize_colon_path_for_bare_tree(raw_path: &str) -> Result<String> {
    let cwd_relative = raw_path.starts_with("./") || raw_path.starts_with("../") || raw_path == ".";
    if cwd_relative {
        return Err(Error::InvalidRef(
            "relative path syntax can't be used outside working tree".to_owned(),
        ));
    }
    let s = raw_path.trim_start_matches('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in s.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            let _ = stack.pop();
        } else {
            stack.push(part);
        }
    }
    Ok(stack.join("/"))
}

fn normalize_colon_path_for_tree(repo: &Repository, raw_path: &str) -> Result<String> {
    let Some(work_tree) = repo.work_tree.as_ref() else {
        return normalize_colon_path_for_bare_tree(raw_path);
    };

    let cwd = std::env::current_dir().map_err(Error::Io)?;
    let wt_canon = work_tree.canonicalize().map_err(Error::Io)?;

    let cwd_relative = raw_path.starts_with("./") || raw_path.starts_with("../") || raw_path == ".";
    if cwd_relative && !path_is_within(&cwd, work_tree) {
        return Err(Error::InvalidRef(
            "relative path syntax can't be used outside working tree".to_owned(),
        ));
    }

    // `./` / `../` / `.` are relative to cwd; other relative paths are relative to work tree.
    let full = if raw_path.starts_with('/') {
        PathBuf::from(raw_path)
    } else if cwd_relative {
        cwd.join(raw_path)
    } else {
        work_tree.join(raw_path)
    };
    let full = normalize_path_components(full);

    if !path_is_within(&full, &wt_canon) {
        return Err(Error::InvalidRef("outside repository".to_owned()));
    }
    let rel = full
        .strip_prefix(&wt_canon)
        .map_err(|_| Error::InvalidRef("outside repository".to_owned()))?;
    let s = rel.to_string_lossy().replace('\\', "/");
    Ok(s.trim_end_matches('/').to_owned())
}

/// Peel tags to a commit OID for merge-base computation (`A...B` and `rev-parse` output).
pub fn peel_to_commit_for_merge_base(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    oid = apply_peel(repo, oid, Some(""))?;
    let obj = repo.read_replaced(&oid)?;
    match obj.kind {
        ObjectKind::Commit => Ok(oid),
        ObjectKind::Tree => Err(Error::InvalidRef(format!(
            "object {oid} does not name a commit"
        ))),
        ObjectKind::Blob => Err(Error::InvalidRef(format!(
            "object {oid} does not name a commit"
        ))),
        ObjectKind::Tag => Err(Error::InvalidRef("unexpected tag after peel".to_owned())),
    }
}

/// Like [`peel_to_commit_for_merge_base`], but returns `Ok(None)` when the peeled object is not a
/// commit (e.g. a tag pointing at a blob). Used by upload-pack fetch negotiation.
pub fn try_peel_to_commit_for_merge_base(
    repo: &Repository,
    oid: ObjectId,
) -> Result<Option<ObjectId>> {
    let oid = apply_peel(repo, oid, Some(""))?;
    let obj = repo.odb.read(&oid)?;
    match obj.kind {
        ObjectKind::Commit => Ok(Some(oid)),
        ObjectKind::Tree | ObjectKind::Blob => Ok(None),
        ObjectKind::Tag => Err(Error::InvalidRef("unexpected tag after peel".to_owned())),
    }
}

/// Peel `oid` to the tree it represents (commits → root tree, tags → recursively, tree → identity).
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] when the object cannot be peeled to a tree (e.g. a blob).
pub fn peel_to_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.read_replaced(&oid)?;
    match obj.kind {
        crate::objects::ObjectKind::Tree => Ok(oid),
        crate::objects::ObjectKind::Commit => {
            let commit = crate::objects::parse_commit(&obj.data)?;
            Ok(commit.tree)
        }
        crate::objects::ObjectKind::Tag => {
            let tag = crate::objects::parse_tag(&obj.data)?;
            peel_to_tree(repo, tag.object)
        }
        _ => Err(Error::ObjectNotFound(format!(
            "cannot peel {} to tree",
            oid
        ))),
    }
}

/// Navigate a tree to find an object at a given path.
///
/// Git accepts `rev:path` when the leaf is a **blob, symlink, gitlink, or tree** (e.g.
/// `HEAD:subdir` for a subdirectory tree, or a submodule path whose leaf is a gitlink). Only
/// [`walk_tree_to_blob_entry`] is blob-only.
fn resolve_tree_path(repo: &Repository, tree_oid: &ObjectId, path: &str) -> Result<ObjectId> {
    resolve_treeish_path_to_object(repo, *tree_oid, path)
}

/// Like Git `rev-parse` for `treeish:path`: the leaf may be a blob or a tree OID.
fn resolve_tree_path_rev_parse(
    repo: &Repository,
    tree_oid: &ObjectId,
    path: &str,
) -> Result<ObjectId> {
    let obj = repo.odb.read(tree_oid)?;
    let entries = crate::objects::parse_tree(&obj.data)?;
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return Err(Error::InvalidRef(format!(
            "path '{path}' does not name an object in tree {tree_oid}"
        )));
    }

    let first = components[0];
    let rest: Vec<&str> = components[1..].to_vec();
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        if name == first {
            if rest.is_empty() {
                // Git's `rev-parse <treeish>:<path>` returns the entry OID for any leaf —
                // blob, tree, symlink, or gitlink. For a gitlink the OID is the submodule's
                // recorded commit SHA, which need not exist in this object store (it lives in
                // the submodule); do not attempt to read it. (lib-submodule-update
                // `test_submodule_content` relies on `rev-parse <commit>:sub1`.)
                return Ok(entry.oid);
            }
            if entry.mode != crate::index::MODE_TREE {
                return Err(Error::ObjectNotFound(path.to_owned()));
            }
            return resolve_tree_path_rev_parse(repo, &entry.oid, &rest.join("/"));
        }
    }
    Err(Error::ObjectNotFound(format!(
        "path '{path}' not found in tree {tree_oid}"
    )))
}

/// Resolved blob (non-tree) at `treeish:path` for diff plumbing.
///
/// Returns the repository-relative path, blob OID, and Git mode string (e.g. `"100644"`).
#[derive(Debug, Clone)]
pub struct TreeishBlobAtPath {
    /// Path used in `diff --git` / `---` / `+++` headers (tree path, `/`-separated).
    pub path: String,
    /// Object id of the blob.
    pub oid: ObjectId,
    /// File mode as in tree objects (`100644`, `100755`, `120000`, …).
    pub mode: String,
}

/// Resolve `rev:path` to the blob at that path in the tree reached from `rev`.
///
/// Fails when `spec` is not `treeish:path`, when the path is missing, or when the
/// target is a tree or gitlink rather than a blob/symlink blob.
pub fn resolve_treeish_blob_at_path(repo: &Repository, spec: &str) -> Result<TreeishBlobAtPath> {
    let (before, after) = split_treeish_colon(spec)
        .ok_or_else(|| Error::InvalidRef(format!("'{spec}' is not a treeish:path revision")))?;

    let rev_oid =
        match resolve_revision_impl(repo, before, true, false, true, true, false, false, true) {
            Ok(o) => o,
            Err(Error::ObjectNotFound(s)) if s == before => {
                return Err(RevParseError::InvalidObjectName {
                    name: before.to_owned(),
                }
                .into());
            }
            Err(Error::RevParse(RevParseError::AmbiguousArgument { .. })) => {
                return Err(RevParseError::InvalidObjectName {
                    name: before.to_owned(),
                }
                .into());
            }
            Err(e) => return Err(e),
        };

    let tree_oid = peel_to_tree(repo, rev_oid)?;

    // Empty path means the root tree itself.
    if after.is_empty() {
        return Ok(TreeishBlobAtPath {
            path: String::new(),
            oid: tree_oid,
            mode: "040000".to_string(),
        });
    }

    let clean_path = match normalize_colon_path_for_tree(repo, after) {
        Ok(p) => p,
        Err(Error::InvalidRef(msg)) if msg == "outside repository" => {
            let wt = repo
                .work_tree
                .as_ref()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            return Err(RevParseError::OutsideRepository {
                path: after.to_owned(),
                work_tree: wt,
            }
            .into());
        }
        Err(e) => return Err(e),
    };

    let (oid, mode_str) = walk_tree_to_blob_entry(repo, &tree_oid, &clean_path)
        .map_err(|e| diagnose_tree_path_error(repo, before, after, &clean_path, e))?;
    Ok(TreeishBlobAtPath {
        path: clean_path,
        oid,
        mode: mode_str,
    })
}

/// Walk from `tree_oid` to the leaf named by `path` and return OID + mode string for a blob or symlink.
///
/// Errors when the leaf is a tree or gitlink. Used by [`resolve_treeish_blob_at_path`] and similar.
fn walk_tree_to_blob_entry(
    repo: &Repository,
    tree_oid: &ObjectId,
    path: &str,
) -> Result<(ObjectId, String)> {
    let obj = repo.read_replaced(tree_oid)?;
    let entries = crate::objects::parse_tree(&obj.data)?;
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return Err(Error::InvalidRef(format!(
            "path '{path}' does not name a blob in tree {tree_oid}"
        )));
    }

    let first = components[0];
    let rest: Vec<&str> = components[1..].to_vec();
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        if name == first {
            if rest.is_empty() {
                if entry.mode == crate::index::MODE_TREE {
                    return Err(Error::InvalidRef(format!("'{path}' is a tree, not a blob")));
                }
                return Ok((entry.oid, entry.mode_str()));
            }
            if entry.mode != crate::index::MODE_TREE {
                return Err(Error::ObjectNotFound(path.to_owned()));
            }
            return walk_tree_to_blob_entry(repo, &entry.oid, &rest.join("/"));
        }
    }
    Err(Error::ObjectNotFound(format!(
        "path '{path}' not found in tree {tree_oid}"
    )))
}

/// A single parent/ancestor navigation step.
#[derive(Debug, Clone, Copy)]
enum NavStep {
    /// `^N` — navigate to the Nth parent (1-indexed; 0 is a no-op).
    ParentN(usize),
    /// `~N` — follow the first parent N times.
    AncestorN(usize),
}

/// Parse and strip any trailing `^N` / `~N` navigation steps from `spec`.
///
/// Returns `(base, steps)` where `steps` are in left-to-right application order.
fn parse_nav_steps(spec: &str) -> (&str, Vec<NavStep>) {
    let mut steps = Vec::new();
    let mut remaining = spec;

    loop {
        // Try `~<digits>` or bare `~` at the end.
        if let Some(tilde_pos) = remaining.rfind('~') {
            let after = &remaining[tilde_pos + 1..];
            if after.is_empty() {
                // bare `~` = `~1`
                steps.push(NavStep::AncestorN(1));
                remaining = &remaining[..tilde_pos];
                continue;
            }
            if after.bytes().all(|b| b.is_ascii_digit()) {
                let n: usize = after.parse().unwrap_or(1);
                steps.push(NavStep::AncestorN(n));
                remaining = &remaining[..tilde_pos];
                continue;
            }
        }

        // Try `^<digits>` or bare `^` at the end (but not `^{...}` — peel strips those first).
        if let Some(caret_pos) = remaining.rfind('^') {
            let after = &remaining[caret_pos + 1..];
            if after.is_empty() {
                // bare `^` = `^1`
                steps.push(NavStep::ParentN(1));
                remaining = &remaining[..caret_pos];
                continue;
            }
            if after.bytes().all(|b| b.is_ascii_digit()) && !after.is_empty() {
                let n: usize = after.parse().unwrap_or(usize::MAX);
                steps.push(NavStep::ParentN(n));
                remaining = &remaining[..caret_pos];
                continue;
            }
        }

        break;
    }

    steps.reverse();
    (remaining, steps)
}

/// Follow annotated tag objects to their peeled target (Git: `^` / `~` peel tags first).
fn peel_annotated_tag_chain(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let obj = repo.read_replaced(&oid)?;
        if obj.kind != ObjectKind::Tag {
            return Ok(oid);
        }
        let tag = parse_tag(&obj.data)?;
        oid = tag.object;
    }
}

/// Apply a single navigation step to an OID, resolving parent/ancestor links.
fn apply_nav_step(repo: &Repository, oid: ObjectId, step: NavStep) -> Result<ObjectId> {
    match step {
        NavStep::ParentN(0) => Ok(oid),
        NavStep::ParentN(n) => {
            let oid = peel_annotated_tag_chain(repo, oid)?;
            let parents = commit_parents_for_navigation(repo, oid)?;
            parents
                .get(n - 1)
                .copied()
                .ok_or_else(|| Error::ObjectNotFound(format!("{oid}^{n}")))
        }
        NavStep::AncestorN(n) => {
            let mut current = peel_annotated_tag_chain(repo, oid)?;
            for _ in 0..n {
                current = apply_nav_step(repo, current, NavStep::ParentN(1))?;
            }
            Ok(current)
        }
    }
}

/// Abbreviate an object ID to a unique prefix.
///
/// The returned prefix is at least `min_len` and at most 40 hex characters.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] when the target OID does not exist in the
/// object database.
pub fn abbreviate_object_id(repo: &Repository, oid: ObjectId, min_len: usize) -> Result<String> {
    let min_len = min_len.clamp(4, 40);
    let target = oid.to_hex();

    // If object doesn't exist, just return the minimum abbreviation
    if !repo.odb.exists(&oid) {
        return Ok(target[..min_len].to_owned());
    }

    let all = collect_loose_object_ids(repo)?;

    for len in min_len..=40 {
        let prefix = &target[..len];
        let matches = all
            .iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .count();
        if matches <= 1 {
            return Ok(prefix.to_owned());
        }
    }

    Ok(target)
}

/// Render `path` relative to `cwd` with `/` separators.
#[must_use]
pub fn to_relative_path(path: &Path, cwd: &Path) -> String {
    let path_components = normalize_components(path);
    let cwd_components = normalize_components(cwd);

    let mut common = 0usize;
    let max_common = path_components.len().min(cwd_components.len());
    while common < max_common && path_components[common] == cwd_components[common] {
        common += 1;
    }

    let mut parts = Vec::new();
    let up_count = cwd_components.len().saturating_sub(common);
    for _ in 0..up_count {
        parts.push("..".to_owned());
    }
    for item in path_components.iter().skip(common) {
        parts.push(item.clone());
    }

    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

fn object_storage_dirs_for_abbrev(repo: &Repository) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let primary = repo.odb.objects_dir().to_path_buf();
    dirs.push(primary.clone());
    if let Ok(alts) = pack::read_alternates_recursive(&primary) {
        for alt in alts {
            if !dirs.iter().any(|d| d == &alt) {
                dirs.push(alt);
            }
        }
    }
    Ok(dirs)
}

fn collect_pack_oids_with_prefix(objects_dir: &Path, prefix: &str) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    for idx in pack::read_local_pack_indexes_cached(objects_dir)? {
        for e in idx.iter() {
            let Ok(oid) = crate::objects::ObjectId::from_bytes(e.oid()) else {
                continue;
            };
            let hex = pack::oid_bytes_to_hex(e.oid());
            if hex.starts_with(prefix) {
                out.push(oid);
            }
        }
    }
    Ok(out)
}

fn disambiguate_kind_rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Tag => 0,
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
    }
}

fn oid_satisfies_peel_filter(repo: &Repository, oid: ObjectId, peel_inner: &str) -> bool {
    apply_peel(repo, oid, Some(peel_inner)).is_ok()
}

/// Candidate objects when a short object id is ambiguous (type order, then hex).
pub fn ambiguous_object_hint_lines(
    repo: &Repository,
    short_prefix: &str,
    peel_filter: Option<&str>,
) -> Result<Vec<AmbiguousObjectHint>> {
    let mut typed: Vec<(u8, String, &'static str)> = Vec::new();
    let mut bad_hex: Vec<String> = Vec::new();
    for oid in list_all_abbrev_matches(repo, short_prefix)? {
        let hex = oid.to_hex();
        match repo.read_replaced(&oid) {
            Ok(obj) => {
                let ok = peel_filter.is_none_or(|p| oid_satisfies_peel_filter(repo, oid, p));
                if ok {
                    typed.push((disambiguate_kind_rank(obj.kind), hex, obj.kind.as_str()));
                }
            }
            Err(_) => bad_hex.push(hex),
        }
    }
    if typed.is_empty() && peel_filter.is_some() {
        return ambiguous_object_hint_lines(repo, short_prefix, None);
    }
    bad_hex.sort();
    typed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = Vec::new();
    for h in bad_hex {
        out.push(AmbiguousObjectHint {
            oid_hex: h,
            kind: None,
        });
    }
    for (_, hex, kind) in typed {
        out.push(AmbiguousObjectHint {
            oid_hex: hex,
            kind: Some(kind),
        });
    }
    Ok(out)
}

fn read_core_disambiguate(repo: &Repository) -> Option<&'static str> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )
    .unwrap_or_else(|_| ConfigSet::new());
    let v = config.get("core.disambiguate")?;
    match v.to_ascii_lowercase().as_str() {
        "committish" | "commit" => Some("commit"),
        "treeish" | "tree" => Some("tree"),
        "blob" => Some("blob"),
        "tag" => Some("tag"),
        "none" => None,
        _ => None,
    }
}

/// When `spec` resolved as an abbreviated object id, warn if `refs/heads/<spec>` exists and
/// points at a different object (Git: `rev-parse` warns "refname ... is ambiguous").
fn warn_if_branch_refname_collides_with_abbrev_hex(
    repo: &Repository,
    spec: &str,
    object_oid: ObjectId,
) {
    if spec.len() >= repo.odb.hash_algo().hex_len() {
        return;
    }
    let branch_ref = format!("refs/heads/{spec}");
    let Ok(ref_oid) = refs::resolve_ref(&repo.git_dir, &branch_ref) else {
        return;
    };
    if ref_oid != object_oid {
        eprintln!("warning: refname '{spec}' is ambiguous.");
    }
}

/// When a hex-like `spec` resolved as a ref under `refs/heads/` or `refs/tags/`, warn if that name
/// also matches object(s) in the ODB (Git: `warning: refname 'abc' is ambiguous.`).
fn warn_if_hex_ref_collides_with_objects(repo: &Repository, spec: &str, ref_oid: ObjectId) {
    if spec.len() >= repo.odb.hash_algo().hex_len() || !is_hex_prefix(spec) {
        return;
    }
    let Ok(matches) = find_abbrev_matches(repo, spec) else {
        return;
    };
    if matches.is_empty() {
        return;
    }
    if matches.len() > 1 || matches[0] != ref_oid {
        eprintln!("warning: refname '{spec}' is ambiguous.");
    }
}

fn disambiguate_hex_by_peel(
    repo: &Repository,
    spec: &str,
    matches: &[ObjectId],
    peel: &str,
) -> Result<ObjectId> {
    let peel_some = Some(peel);
    let filtered: Vec<ObjectId> = matches
        .iter()
        .copied()
        .filter(|oid| apply_peel(repo, *oid, peel_some).is_ok())
        .collect();
    if filtered.len() == 1 {
        return Ok(filtered[0]);
    }
    if filtered.is_empty() {
        return Err(Error::InvalidRef(format!(
            "short object ID {spec} is ambiguous"
        )));
    }
    let mut peeled_targets: HashSet<ObjectId> = HashSet::new();
    for oid in &filtered {
        if let Ok(p) = apply_peel(repo, *oid, peel_some) {
            peeled_targets.insert(p);
        }
    }
    if peeled_targets.len() == 1 {
        // Several objects (e.g. commit + tag) may peel to the same commit; any representative
        // is valid for subsequent `apply_peel` in `resolve_revision_impl`.
        let mut sorted = filtered;
        sorted.sort_by_key(|o| o.to_hex());
        return Ok(sorted[0]);
    }
    // `^{commit}`: multiple objects may peel to the same commit (e.g. HEAD, tag, peeled tree-ish).
    // If exactly one distinct commit is produced, pick a deterministic representative (t1512).
    if peel == "commit" {
        let mut by_peeled: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        for oid in &filtered {
            if let Ok(c) = apply_peel(repo, *oid, Some("commit")) {
                by_peeled.entry(c).or_default().push(*oid);
            }
        }
        if by_peeled.len() == 1 {
            let mut reps: Vec<ObjectId> = by_peeled.into_values().next().unwrap_or_default();
            reps.sort_by_key(|o| o.to_hex());
            if let Some(oid) = reps.first().copied() {
                return Ok(oid);
            }
        }
    }
    Err(Error::InvalidRef(format!(
        "short object ID {spec} is ambiguous"
    )))
}

fn commit_reachable_closure(repo: &Repository, start: ObjectId) -> Result<HashSet<ObjectId>> {
    use std::collections::VecDeque;
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([start]);
    while let Some(oid) = q.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let obj = match repo.read_replaced(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = match parse_commit(&obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for p in &commit.parents {
            q.push_back(*p);
        }
    }
    Ok(seen)
}

/// `git rev-list --count <tag>..<head>` — commits reachable from `head` but not from `tag`.
fn describe_generation_count(
    repo: &Repository,
    head: ObjectId,
    tag_commit: ObjectId,
) -> Result<usize> {
    let from_tag = commit_reachable_closure(repo, tag_commit)?;
    let from_head = commit_reachable_closure(repo, head)?;
    Ok(from_head.difference(&from_tag).count())
}

fn try_resolve_describe_name(repo: &Repository, spec: &str) -> Result<Option<ObjectId>> {
    let re = Regex::new(r"(?i)^(.+)-(\d+)-g([0-9a-fA-F]+)$")
        .map_err(|_| Error::from(RevParseError::InternalDescribeRegex))?;
    let Some(caps) = re.captures(spec) else {
        return Ok(None);
    };
    let tag_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let gen: usize = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let hex_abbrev = caps.get(3).map(|m| m.as_str()).unwrap_or("");
    if tag_name.is_empty() || hex_abbrev.is_empty() {
        return Ok(None);
    }
    let hex_lower = hex_abbrev.to_ascii_lowercase();
    let mut commit_candidates: Vec<ObjectId> = find_abbrev_matches(repo, &hex_lower)?
        .into_iter()
        .filter(|oid| {
            repo.odb
                .read(oid)
                .is_ok_and(|o| o.kind == ObjectKind::Commit)
        })
        .collect();
    commit_candidates.sort_by_key(|o| o.to_hex());
    commit_candidates.dedup();

    if let Ok(tag_oid) = refs::resolve_ref(&repo.git_dir, &format!("refs/tags/{tag_name}"))
        .or_else(|_| refs::resolve_ref(&repo.git_dir, tag_name))
    {
        let tag_commit = peel_to_commit_for_merge_base(repo, tag_oid)?;
        let mut strict_candidates = commit_candidates
            .iter()
            .copied()
            .filter(|oid| describe_generation_count(repo, *oid, tag_commit).ok() == Some(gen))
            .collect::<Vec<_>>();
        strict_candidates.sort_by_key(|o| o.to_hex());
        if strict_candidates.len() == 1 {
            return Ok(Some(strict_candidates[0]));
        }
        if strict_candidates.len() > 1 {
            return Err(Error::InvalidRef(format!(
                "short object ID {hex_abbrev} is ambiguous"
            )));
        }
    }

    match commit_candidates.len() {
        0 => Err(Error::ObjectNotFound(spec.to_owned())),
        1 => Ok(Some(commit_candidates[0])),
        _ => Err(Error::InvalidRef(format!(
            "short object ID {hex_abbrev} is ambiguous"
        ))),
    }
}

#[expect(clippy::too_many_arguments, clippy::only_used_in_recursion)]
fn resolve_base(
    repo: &Repository,
    spec: &str,
    index_dwim: bool,
    commit_only_hex: bool,
    use_disambiguate_config: bool,
    peel_for_disambig: Option<&str>,
    implicit_tree_abbrev: bool,
    implicit_blob_abbrev: bool,
    remote_branch_name_guess: bool,
) -> Result<ObjectId> {
    // Standalone `@` is an alias for `HEAD` in revision parsing.
    if spec == "@" {
        return resolve_base(
            repo,
            "HEAD",
            index_dwim,
            commit_only_hex,
            use_disambiguate_config,
            peel_for_disambig,
            implicit_tree_abbrev,
            implicit_blob_abbrev,
            remote_branch_name_guess,
        );
    }

    // `FETCH_HEAD`: prefer the first for-merge line, but allow checkout-style consumers to use
    // the first fetched OID even when every line is marked `not-for-merge`.
    if spec == "FETCH_HEAD" {
        let path = repo.git_dir.join("FETCH_HEAD");
        let content = std::fs::read_to_string(&path)
            .map_err(|_| Error::ObjectNotFound("FETCH_HEAD".to_owned()))?;
        let mut first_oid = None;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split('\t');
            let Some(oid_hex) = parts.next() else {
                continue;
            };
            if ObjectId::is_full_hex(oid_hex) {
                let oid = oid_hex
                    .parse::<ObjectId>()
                    .map_err(|_| Error::InvalidRef("invalid FETCH_HEAD object id".to_owned()))?;
                first_oid.get_or_insert(oid);
                let not_for_merge = parts.next().is_some_and(|v| v == "not-for-merge");
                if !not_for_merge {
                    return Ok(oid);
                }
            }
        }
        return first_oid.ok_or_else(|| Error::ObjectNotFound("FETCH_HEAD".to_owned()));
    }

    // `@{-N}` must run before reflog parsing so `@{-1}@{1}` is not misread as `@{-1}` + `@{1}`.
    if let Some(after_open) = spec.strip_prefix("@{-") {
        if let Some(close) = after_open.find('}') {
            let n_str = &after_open[..close];
            if let Ok(n) = n_str.parse::<usize>() {
                if n >= 1 {
                    let suffix = &after_open[close + 1..];
                    if suffix.is_empty() {
                        if let Some(oid) = try_resolve_at_minus(repo, spec)? {
                            return Ok(oid);
                        }
                    } else {
                        let branch = resolve_at_minus_to_branch(repo, n)?;
                        let new_spec = format!("{branch}{suffix}");
                        return resolve_base(
                            repo,
                            &new_spec,
                            index_dwim,
                            commit_only_hex,
                            use_disambiguate_config,
                            peel_for_disambig,
                            implicit_tree_abbrev,
                            implicit_blob_abbrev,
                            remote_branch_name_guess,
                        );
                    }
                }
            }
        }
    }

    // Handle @{upstream} / @{u} / @{push} suffixes (including compounds like branch@{u}@{1})
    if upstream_suffix_info(spec).is_some() {
        let full_ref = resolve_upstream_symbolic_name(repo, spec)?;
        return refs::resolve_ref(&repo.git_dir, &full_ref)
            .map_err(|_| Error::ObjectNotFound(spec.to_owned()));
    }

    // Reflog selectors: `main@{1}`, `@{3}` (current branch), `other@{u}@{1}`, etc.
    if let Some(oid) = try_resolve_reflog_index(repo, spec)? {
        return Ok(oid);
    }

    // Handle `:/pattern` — search commit messages from HEAD
    if let Some(pattern) = spec.strip_prefix(":/") {
        if !pattern.is_empty() {
            return resolve_commit_message_search(repo, pattern);
        }
    }

    // Handle `:N:path` — look up path in the index at stage N
    // Also handle `:path` — look up path in the index (stage 0)
    if let Some(rest) = spec.strip_prefix(':') {
        if !rest.is_empty() && !rest.starts_with('/') {
            // Check for :N:path pattern (N is a single digit 0-3)
            if rest.len() >= 3 && rest.as_bytes()[1] == b':' {
                if let Some(stage_char) = rest.chars().next() {
                    if let Some(stage) = stage_char.to_digit(10) {
                        if stage <= 3 {
                            let raw_path = &rest[2..];
                            let path = match normalize_colon_path_for_tree(repo, raw_path) {
                                Ok(p) => p,
                                Err(Error::InvalidRef(msg)) if msg == "outside repository" => {
                                    let wt = repo
                                        .work_tree
                                        .as_ref()
                                        .and_then(|p| p.canonicalize().ok())
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_default();
                                    return Err(RevParseError::OutsideRepository {
                                        path: raw_path.to_owned(),
                                        work_tree: wt,
                                    }
                                    .into());
                                }
                                Err(e) => return Err(e),
                            };
                            return resolve_index_path_at_stage(repo, &path, stage as u8).map_err(
                                |e| diagnose_index_path_error(repo, &path, stage as u8, e),
                            );
                        }
                    }
                }
            }
            let clean_rest = match normalize_colon_path_for_tree(repo, rest) {
                Ok(p) => p,
                Err(Error::InvalidRef(msg)) if msg == "outside repository" => {
                    let wt = repo
                        .work_tree
                        .as_ref()
                        .and_then(|p| p.canonicalize().ok())
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    return Err(RevParseError::OutsideRepository {
                        path: rest.to_owned(),
                        work_tree: wt,
                    }
                    .into());
                }
                Err(e) => return Err(e),
            };
            return resolve_index_path(repo, &clean_rest)
                .map_err(|e| diagnose_index_path_error(repo, &clean_rest, 0, e));
        }
    }

    if let Some((treeish, path)) = split_treeish_spec(spec) {
        let root_oid = resolve_revision_impl(
            repo,
            treeish,
            index_dwim,
            commit_only_hex,
            use_disambiguate_config,
            false,
            false,
            false,
            false,
        )?;
        return resolve_treeish_path_to_object(repo, root_oid, path);
    }

    if let Ok(oid) = spec.parse::<ObjectId>() {
        // A full 40-hex OID is always accepted, even if the object
        // doesn't exist in the ODB (matches git behavior).
        let rn = format!("refs/heads/{spec}");
        if refs::resolve_ref(&repo.git_dir, &rn).is_ok() {
            eprintln!("warning: refname '{spec}' is ambiguous.");
        }
        return Ok(oid);
    }

    match try_resolve_describe_name(repo, spec) {
        Ok(Some(oid)) => return Ok(oid),
        Err(e) => return Err(e),
        Ok(None) => {}
    }

    // Hex-like tokens may name refs (e.g. tag `1.2` / `2.2`) — resolve those before treating the
    // string as an abbreviated object id (t5334 incremental MIDX).
    if is_hex_prefix(spec) && spec.len() < repo.odb.hash_algo().hex_len() {
        let tag_ref = format!("refs/tags/{spec}");
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &tag_ref) {
            warn_if_hex_ref_collides_with_objects(repo, spec, oid);
            return Ok(oid);
        }
        let branch_ref = format!("refs/heads/{spec}");
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &branch_ref) {
            warn_if_hex_ref_collides_with_objects(repo, spec, oid);
            return Ok(oid);
        }
    }

    if is_hex_prefix(spec) {
        let matches = find_abbrev_matches(repo, spec)?;
        if matches.is_empty() {
            // Git treats 4+ hex digits as an abbreviated object id lookup first. When nothing
            // matches, fail as unknown revision — do not fall through to index DWIM (which would
            // incorrectly report "ambiguous argument" for paths like `000000000`).
            if (4..repo.odb.hash_algo().hex_len()).contains(&spec.len()) {
                return Err(Error::ObjectNotFound(spec.to_owned()));
            }
        } else if matches.len() == 1 {
            let oid = matches[0];
            warn_if_branch_refname_collides_with_abbrev_hex(repo, spec, oid);
            return Ok(oid);
        } else if matches.len() > 1 {
            if let Some(p) = peel_for_disambig {
                let oid = disambiguate_hex_by_peel(repo, spec, &matches, p)?;
                warn_if_branch_refname_collides_with_abbrev_hex(repo, spec, oid);
                return Ok(oid);
            }
            if commit_only_hex {
                let oid = disambiguate_hex_by_peel(repo, spec, &matches, "commit")?;
                warn_if_branch_refname_collides_with_abbrev_hex(repo, spec, oid);
                return Ok(oid);
            }
            if use_disambiguate_config {
                if let Some(pref) = read_core_disambiguate(repo) {
                    if let Ok(oid) = disambiguate_hex_by_peel(repo, spec, &matches, pref) {
                        warn_if_branch_refname_collides_with_abbrev_hex(repo, spec, oid);
                        return Ok(oid);
                    }
                }
            }
            return Err(Error::InvalidRef(format!(
                "short object ID {} is ambiguous",
                spec
            )));
        }
    }

    let (dwim_count, dwim_oid) = resolve_ref_dwim_for_rev_parse(repo, spec);
    if dwim_count > 1 {
        eprintln!("warning: refname '{spec}' is ambiguous.");
    }
    if let Some(oid) = dwim_oid {
        return Ok(oid);
    }
    // `remotes/<remote>/<ref>` is a common shorthand for `refs/remotes/<remote>/<ref>` (t2024).
    if let Some(rest) = spec.strip_prefix("remotes/") {
        let full = format!("refs/remotes/{rest}");
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &full) {
            return Ok(oid);
        }
    }
    // Remote name alone (`origin`, `upstream`): resolve like Git via
    // `refs/remotes/<name>/HEAD` (symref to the default remote-tracking branch).
    // Skip when a local branch with the same short name exists.
    if !spec.contains('/')
        && !spec.starts_with('.')
        && spec != "HEAD"
        && spec != "FETCH_HEAD"
        && spec != "MERGE_HEAD"
        && spec != "CHERRY_PICK_HEAD"
        && spec != "REVERT_HEAD"
        && spec != "REBASE_HEAD"
        && spec != "AUTO_MERGE"
        && spec != "stash"
    {
        let local_branch = format!("refs/heads/{spec}");
        if refs::resolve_ref(&repo.git_dir, &local_branch).is_err() {
            let remote_head = format!("refs/remotes/{spec}/HEAD");
            if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &remote_head) {
                return Ok(oid);
            }
        }
    }
    // DWIM: bare `stash` refers to `refs/stash` (like upstream Git), not `.git/stash`.
    if spec == "stash" {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, "refs/stash") {
            return Ok(oid);
        }
    }
    // Short names: resolve `refs/heads/<spec>` and `refs/tags/<spec>`. When both exist and
    // disagree, prefer the branch (matches `git checkout` / `git reset` for names like `b1`)
    // and warn, matching upstream ambiguous-refname behavior.
    let head_ref = format!("refs/heads/{spec}");
    let tag_ref = format!("refs/tags/{spec}");
    let head_oid = refs::resolve_ref(&repo.git_dir, &head_ref).ok();
    let tag_oid = refs::resolve_ref(&repo.git_dir, &tag_ref).ok();
    match (head_oid, tag_oid) {
        (Some(h), Some(t)) if h != t => {
            eprintln!("warning: refname '{spec}' is ambiguous.");
            return Ok(h);
        }
        (Some(h), _) => return Ok(h),
        (None, Some(t)) => return Ok(t),
        (None, None) => {}
    }

    // `rev-parse` / `pack-objects --revs`: when `spec` is a single path component and a ref of
    // that basename exists (`refs/tags/A` vs worktree file `A.t`), prefer the ref over index
    // DWIM (matches Git; t5332).
    if !spec.contains('/')
        && !spec.contains(':')
        && !spec.starts_with('.')
        && spec != "HEAD"
        && spec.len() <= 255
    {
        let mut ref_match: Option<ObjectId> = None;
        for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/", "refs/notes/"] {
            let full = format!("{prefix}{spec}");
            if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &full) {
                ref_match = Some(oid);
                break;
            }
        }
        if let Some(oid) = ref_match {
            return Ok(oid);
        }
    }
    for candidate in &[format!("refs/remotes/{spec}"), format!("refs/notes/{spec}")] {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, candidate) {
            return Ok(oid);
        }
    }

    // `git log one` / `git rev-parse one`: remote name → `refs/remotes/<name>/HEAD` (Git DWIM).
    if let Some(head_ref) = remote_tracking_head_symbolic_target(repo, spec) {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &head_ref) {
            return Ok(oid);
        }
    }

    // DWIM: `checkout B2` when only `refs/remotes/origin/B2` exists (common after `fetch`).
    if remote_branch_name_guess
        && !spec.contains('/')
        && spec != "HEAD"
        && spec != "FETCH_HEAD"
        && spec != "MERGE_HEAD"
    {
        const REMOTES: &str = "refs/remotes/";
        if let Ok(remote_refs) = refs::list_refs(&repo.git_dir, REMOTES) {
            let matches: Vec<ObjectId> = remote_refs
                .into_iter()
                .filter(|(r, _)| {
                    r.strip_prefix(REMOTES)
                        .is_some_and(|rest| rest == spec || rest.ends_with(&format!("/{spec}")))
                })
                .map(|(_, oid)| oid)
                .collect();
            if matches.len() == 1 {
                return Ok(matches[0]);
            }
            if matches.len() > 1 {
                return Err(Error::InvalidRef(format!(
                    "ambiguous refname '{spec}': matches multiple remote-tracking branches"
                )));
            }
        }
    }

    // As a last resort, try resolving as an index path (porcelain / DWIM only).
    if !spec.contains(':') && !spec.starts_with('-') {
        if index_dwim {
            if let Ok(oid) = resolve_index_path(repo, spec) {
                return Ok(oid);
            }
        }
        return Err(RevParseError::AmbiguousArgument {
            spec: spec.to_owned(),
        }
        .into());
    }
    Err(Error::ObjectNotFound(spec.to_owned()))
}

/// Resolve `@{-N}` to the branch name (e.g. "side"), not to an OID.
fn resolve_at_minus_to_branch(repo: &Repository, n: usize) -> Result<String> {
    let entries = read_reflog(&repo.git_dir, "HEAD")?;
    let mut count = 0usize;
    for entry in entries.iter().rev() {
        let msg = &entry.message;
        if let Some(rest) = msg.strip_prefix("checkout: moving from ") {
            count += 1;
            if count == n {
                if let Some(to_pos) = rest.find(" to ") {
                    return Ok(rest[..to_pos].to_string());
                }
            }
        }
    }
    Err(Error::InvalidRef(format!(
        "@{{-{n}}}: only {count} checkout(s) in reflog"
    )))
}

/// Try to resolve `@{-N}` syntax — the Nth previously checked out branch.
/// Returns the resolved OID if matching, or None if not matching.
fn try_resolve_at_minus(repo: &Repository, spec: &str) -> Result<Option<ObjectId>> {
    // Match @{-N} only (no ref prefix)
    if !spec.starts_with("@{-") || !spec.ends_with('}') {
        return Ok(None);
    }
    let inner = &spec[3..spec.len() - 1];
    let n: usize = match inner.parse() {
        Ok(n) if n >= 1 => n,
        _ => return Ok(None),
    };
    // Read HEAD reflog and find the Nth "checkout: moving from X to Y" entry
    let entries = read_reflog(&repo.git_dir, "HEAD")?;
    let mut count = 0usize;
    // Iterate newest-first
    for entry in entries.iter().rev() {
        let msg = &entry.message;
        if let Some(rest) = msg.strip_prefix("checkout: moving from ") {
            count += 1;
            if count == n {
                if let Some(to_pos) = rest.find(" to ") {
                    let from_branch = &rest[..to_pos];
                    let ref_name = format!("refs/heads/{from_branch}");
                    if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &ref_name) {
                        return Ok(Some(oid));
                    }
                    if let Ok(oid) = from_branch.parse::<ObjectId>() {
                        if repo.odb.exists(&oid) {
                            return Ok(Some(oid));
                        }
                    }
                    if is_hex_prefix(from_branch) {
                        if let Ok(oid) = resolve_revision_for_range_end(repo, from_branch)
                            .and_then(|oid| peel_to_commit_for_merge_base(repo, oid))
                        {
                            return Ok(Some(oid));
                        }
                    }
                    return Err(Error::InvalidRef(format!(
                        "cannot resolve @{{-{n}}}: branch '{}' not found",
                        from_branch
                    )));
                }
            }
        }
    }
    Err(Error::InvalidRef(format!(
        "@{{-{n}}}: only {count} checkout(s) in reflog"
    )))
}

#[derive(Debug, Clone)]
enum AtStep {
    Index(usize),
    Date(i64),
    Upstream,
    Push,
    Now,
}

fn try_parse_at_step_inner(inner: &str) -> Option<AtStep> {
    if inner.eq_ignore_ascii_case("u") || inner.eq_ignore_ascii_case("upstream") {
        return Some(AtStep::Upstream);
    }
    if inner.eq_ignore_ascii_case("push") {
        return Some(AtStep::Push);
    }
    if inner.eq_ignore_ascii_case("now") {
        return Some(AtStep::Now);
    }
    if let Ok(n) = inner.parse::<usize>() {
        return Some(AtStep::Index(n));
    }
    approxidate(inner).map(AtStep::Date)
}

fn next_reflog_at_open(spec: &str, mut from: usize) -> Option<usize> {
    let b = spec.as_bytes();
    while let Some(rel) = spec[from..].find("@{") {
        let i = from + rel;
        // `@{-N}` is previous-branch syntax, not a reflog selector — skip the whole token.
        if b.get(i + 2) == Some(&b'-') {
            let after_open = i + 2;
            let close = spec[after_open..].find('}').map(|j| after_open + j)?;
            from = close + 1;
            continue;
        }
        return Some(i);
    }
    None
}

/// Split `spec` into a ref prefix and a chain of `@{...}` steps (empty chain → not a reflog form).
fn split_reflog_at_chain(spec: &str) -> Option<(String, Vec<AtStep>)> {
    let at = next_reflog_at_open(spec, 0)?;
    let prefix = spec[..at].to_owned();
    let mut steps = Vec::new();
    let mut pos = at;
    while pos < spec.len() {
        let rest = &spec[pos..];
        if !rest.starts_with("@{") {
            return None;
        }
        if rest.as_bytes().get(2) == Some(&b'-') {
            return None;
        }
        let inner_start = pos + 2;
        let close = spec[inner_start..].find('}').map(|i| inner_start + i)?;
        let inner = &spec[inner_start..close];
        let step = try_parse_at_step_inner(inner)?;
        steps.push(step);
        pos = close + 1;
    }
    if steps.is_empty() {
        return None;
    }
    Some((prefix, steps))
}

fn dwim_refname(repo: &Repository, raw: &str) -> String {
    if raw.is_empty() || raw == "HEAD" || raw.starts_with("refs/") {
        return raw.to_owned();
    }
    // Bare `stash` is `refs/stash` (not `refs/heads/stash`); reflog lives at `logs/refs/stash`.
    if raw == "stash" && refs::resolve_ref(&repo.git_dir, "refs/stash").is_ok() {
        return "refs/stash".to_owned();
    }
    let candidate = format!("refs/heads/{raw}");
    if refs::resolve_ref(&repo.git_dir, &candidate).is_ok() {
        candidate
    } else {
        raw.to_owned()
    }
}

fn reflog_display_name(refname_raw: &str, refname: &str) -> String {
    if refname_raw.is_empty() {
        if let Some(b) = refname.strip_prefix("refs/heads/") {
            return b.to_owned();
        }
        return refname.to_owned();
    }
    refname_raw.to_owned()
}

fn resolve_reflog_oid(
    repo: &Repository,
    refname: &str,
    refname_raw: &str,
    index_or_date: ReflogSelector,
) -> Result<ObjectId> {
    let mut entries = read_reflog(&repo.git_dir, refname)?;
    if refname == "HEAD" {
        if let ReflogSelector::Index(index) = index_or_date {
            if index >= entries.len() {
                if let Ok(Some(branch_ref)) = crate::refs::read_symbolic_ref(&repo.git_dir, "HEAD")
                {
                    if let Ok(branch_entries) = read_reflog(&repo.git_dir, &branch_ref) {
                        if index < branch_entries.len() {
                            entries = branch_entries;
                        }
                    }
                }
            }
        }
    }
    let display = reflog_display_name(refname_raw, refname);
    match index_or_date {
        ReflogSelector::Index(index) => {
            let len = entries.len();
            if index == 0 {
                if len == 0 {
                    // Git's `repo_dwim_log` only treats `ref@{N}` as resolvable when the
                    // reflog file actually exists; with no reflog at all the resolution
                    // fails. Only when the reflog exists but is empty does `ref@{0}` fall
                    // back to the ref's current value (the `nth == co_cnt` case in
                    // object-name.c).
                    if !crate::reflog::reflog_exists(&repo.git_dir, refname) {
                        return Err(RevParseError::ReflogEmpty {
                            ref_display: display.clone(),
                        }
                        .into());
                    }
                    return refs::resolve_ref(&repo.git_dir, refname).map_err(|_| {
                        RevParseError::ReflogEmpty {
                            ref_display: display.clone(),
                        }
                        .into()
                    });
                }
                return Ok(entries[len - 1].new_oid);
            }
            if len == 0 {
                return Err(RevParseError::ReflogEmpty {
                    ref_display: display.clone(),
                }
                .into());
            }
            if index > len {
                return Err(RevParseError::ReflogInsufficientEntries {
                    ref_display: display.clone(),
                    available: len,
                }
                .into());
            }
            let oid = entries[len - index].old_oid;
            if oid.is_zero() {
                return Err(RevParseError::ReflogInsufficientEntries {
                    ref_display: display.clone(),
                    available: len,
                }
                .into());
            }
            Ok(oid)
        }
        ReflogSelector::Date(target_ts) => {
            if entries.is_empty() {
                return Err(RevParseError::ReflogEmpty {
                    ref_display: display.clone(),
                }
                .into());
            }
            for entry in entries.iter().rev() {
                let ts = parse_reflog_entry_timestamp(entry);
                if let Some(t) = ts {
                    if t <= target_ts {
                        return Ok(entry.new_oid);
                    }
                }
            }
            Ok(entries[0].new_oid)
        }
    }
}

fn resolve_at_minus_token_to_branch(repo: &Repository, token: &str) -> Result<Option<String>> {
    if !token.starts_with("@{-") || !token.ends_with('}') {
        return Ok(None);
    }
    let inner = &token[3..token.len() - 1];
    let n: usize = inner
        .parse()
        .map_err(|_| Error::InvalidRef(format!("invalid N in @{{-N}} for '{token}'")))?;
    if n < 1 {
        return Ok(None);
    }
    Ok(Some(resolve_at_minus_to_branch(repo, n)?))
}

/// Ref whose reflog `git log -g` should walk for a revision like `other@{u}` or `main@{1}`.
///
/// Returns `None` when `spec` is not a reflog-chain form (no `@{` step after the prefix).
pub fn reflog_walk_refname(repo: &Repository, spec: &str) -> Result<Option<String>> {
    let Some((prefix, steps)) = split_reflog_at_chain(spec) else {
        return Ok(None);
    };

    let prefix_resolved = if let Some(b) = resolve_at_minus_token_to_branch(repo, &prefix)? {
        b
    } else {
        prefix.clone()
    };

    let mut current_spec = if prefix_resolved.is_empty() {
        if let Ok(Some(b)) = refs::read_head(&repo.git_dir) {
            if let Some(short) = b.strip_prefix("refs/heads/") {
                short.to_owned()
            } else {
                "HEAD".to_owned()
            }
        } else {
            "HEAD".to_owned()
        }
    } else {
        prefix_resolved
    };

    let last_reflog_peel = steps
        .iter()
        .rposition(|s| matches!(s, AtStep::Index(_) | AtStep::Date(_) | AtStep::Now));

    let limit = last_reflog_peel.unwrap_or(steps.len());
    for step in steps.iter().take(limit) {
        match step {
            AtStep::Upstream => {
                let base = if current_spec == "@" {
                    "HEAD"
                } else {
                    current_spec.as_str()
                };
                let full = resolve_upstream_symbolic_name(repo, &format!("{base}@{{u}}"))?;
                current_spec = full;
            }
            AtStep::Push => {
                let base = if current_spec == "@" {
                    "HEAD"
                } else {
                    current_spec.as_str()
                };
                let full = resolve_upstream_symbolic_name(repo, &format!("{base}@{{push}}"))?;
                current_spec = full;
            }
            AtStep::Now | AtStep::Index(_) | AtStep::Date(_) => {}
        }
    }

    Ok(Some(dwim_refname(repo, current_spec.as_str())))
}

/// Resolve a user revision string to the reflog file ref name for `log -g` / `rev-list -g`.
///
/// Mirrors Git `add_reflog_for_walk` / `read_complete_reflog` ref resolution before reading
/// `logs/<ref>`.
pub fn resolve_reflog_walk_log_ref(repo: &Repository, r: &str) -> Result<String> {
    if let Ok(Some(w)) = reflog_walk_refname(repo, r) {
        return Ok(w);
    }
    if r == "HEAD" || r.starts_with("refs/") {
        return Ok(r.to_string());
    }
    if r.starts_with("@{") {
        if let Some(n_str) = r.strip_prefix("@{").and_then(|s| s.strip_suffix('}')) {
            if let Some(stripped) = n_str.strip_prefix('-') {
                if stripped.parse::<usize>().is_ok() {
                    if let Ok(branch) = refs::resolve_at_n_branch(&repo.git_dir, r) {
                        return Ok(format!("refs/heads/{branch}"));
                    }
                }
            }
        }
        return Ok(r.to_string());
    }
    let candidate = format!("refs/heads/{r}");
    if refs::resolve_ref(&repo.git_dir, &candidate).is_ok() {
        Ok(candidate)
    } else {
        Ok(r.to_string())
    }
}

/// Try to resolve `ref@{...}` with optional chained `@{...}` steps (e.g. `other@{u}@{1}`).
fn try_resolve_reflog_index(repo: &Repository, spec: &str) -> Result<Option<ObjectId>> {
    let Some((prefix, steps)) = split_reflog_at_chain(spec) else {
        return Ok(None);
    };

    let prefix_resolved = if let Some(b) = resolve_at_minus_token_to_branch(repo, &prefix)? {
        b
    } else {
        prefix.clone()
    };

    let mut current_spec = if prefix_resolved.is_empty() {
        if let Ok(Some(b)) = refs::read_head(&repo.git_dir) {
            if let Some(short) = b.strip_prefix("refs/heads/") {
                short.to_owned()
            } else {
                "HEAD".to_owned()
            }
        } else {
            "HEAD".to_owned()
        }
    } else {
        prefix_resolved
    };

    for (i, step) in steps.iter().enumerate() {
        match step {
            AtStep::Upstream => {
                let base = if current_spec == "@" {
                    "HEAD"
                } else {
                    current_spec.as_str()
                };
                let full = resolve_upstream_symbolic_name(repo, &format!("{base}@{{u}}"))?;
                current_spec = full;
            }
            AtStep::Push => {
                let base = if current_spec == "@" {
                    "HEAD"
                } else {
                    current_spec.as_str()
                };
                let full = resolve_upstream_symbolic_name(repo, &format!("{base}@{{push}}"))?;
                current_spec = full;
            }
            AtStep::Now => {
                let refname_raw = current_spec.as_str();
                let refname = dwim_refname(repo, refname_raw);
                let oid =
                    resolve_reflog_oid(repo, &refname, refname_raw, ReflogSelector::Index(0))?;
                if i + 1 == steps.len() {
                    return Ok(Some(oid));
                }
                current_spec = oid.to_hex();
            }
            AtStep::Index(n) => {
                let refname_raw = current_spec.as_str();
                let refname = dwim_refname(repo, refname_raw);
                let oid =
                    resolve_reflog_oid(repo, &refname, refname_raw, ReflogSelector::Index(*n))?;
                if i + 1 == steps.len() {
                    return Ok(Some(oid));
                }
                current_spec = oid.to_hex();
            }
            AtStep::Date(ts) => {
                let refname_raw = current_spec.as_str();
                let refname = dwim_refname(repo, refname_raw);
                let oid =
                    resolve_reflog_oid(repo, &refname, refname_raw, ReflogSelector::Date(*ts))?;
                if i + 1 == steps.len() {
                    return Ok(Some(oid));
                }
                current_spec = oid.to_hex();
            }
        }
    }

    let refname_raw = current_spec.as_str();
    let refname = dwim_refname(repo, refname_raw);
    refs::resolve_ref(&repo.git_dir, &refname)
        .map(Some)
        .map_err(|_| Error::ObjectNotFound(spec.to_owned()))
}

enum ReflogSelector {
    Index(usize),
    Date(i64),
}

/// Parse a timestamp from a reflog entry's identity string.
fn parse_reflog_entry_timestamp(entry: &crate::reflog::ReflogEntry) -> Option<i64> {
    // Identity looks like: "Name <email> 1234567890 +0000"
    let parts: Vec<&str> = entry.identity.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse::<i64>().ok()
    } else {
        None
    }
}

/// Parse a reflog date selector string (e.g. `yesterday`, `2005-04-07`) to a Unix timestamp.
///
/// Used by `git log -g` display to match Git's `ref@{date}` formatting in tests.
#[must_use]
pub fn reflog_date_selector_timestamp(s: &str) -> Option<i64> {
    approxidate(s)
}

/// Simple approximate date parser for reflog date lookups.
/// Handles formats like "2001-09-17", "3.hot.dogs.on.2001-09-17", etc.
fn approxidate(s: &str) -> Option<i64> {
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let lower = s.trim().to_ascii_lowercase();
    if lower.split_whitespace().next() == Some("now") {
        // Match Git's test harness: `test_tick` sets GIT_COMMITTER_DATE; `@{now}` must use that
        // clock, not wall time (t1507 `log -g other@{u}@{now}`).
        if let Ok(raw) =
            std::env::var("GIT_COMMITTER_DATE").or_else(|_| std::env::var("GIT_AUTHOR_DATE"))
        {
            let mut it = raw.split_whitespace();
            if let Some(ts) = it.next().and_then(|p| p.parse::<i64>().ok()) {
                return Some(ts);
            }
        }
        return Some(now_ts);
    }
    if let Ok((ts, _offset)) = crate::git_date::parse::parse_date_basic(s.trim()) {
        return Some(ts as i64);
    }
    // Handle relative time: "N.unit.ago" or "N unit ago"
    // e.g. "1.year.ago", "2.weeks.ago", "3 hours ago"
    let relative = lower.replace('.', " ");
    let parts: Vec<&str> = relative.split_whitespace().collect();
    if parts.len() >= 2 {
        // Try to parse "N unit ago" or just "N unit". Both are past-relative: git's
        // approxidate treats a bare "N unit" the same as "N unit ago" (it parses times for
        // --since/--until), so the result is always `now - N*unit`.
        let (n_str, unit) = if (parts.len() >= 3 && parts[2] == "ago") || parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            ("", "")
        };
        if !n_str.is_empty() {
            if let Ok(n) = n_str.parse::<i64>() {
                let secs: Option<i64> = match unit.trim_end_matches('s') {
                    "second" => Some(n),
                    "minute" => Some(n * 60),
                    "hour" => Some(n * 3600),
                    "day" => Some(n * 86400),
                    "week" => Some(n * 604800),
                    "month" => Some(n * 2592000),
                    "year" => Some(n * 31536000),
                    _ => None,
                };
                if let Some(s) = secs {
                    return Some(now_ts - s);
                }
            }
        }
    }
    // Try to extract a YYYY-MM-DD pattern from the string
    let re_like = |input: &str| -> Option<i64> {
        // Scan for 4-digit year followed by -MM-DD
        for (i, _) in input.char_indices() {
            let rest = &input[i..];
            if rest.len() >= 10 {
                let bytes = rest.as_bytes();
                if bytes[4] == b'-'
                    && bytes[7] == b'-'
                    && bytes[0..4].iter().all(|b| b.is_ascii_digit())
                    && bytes[5..7].iter().all(|b| b.is_ascii_digit())
                    && bytes[8..10].iter().all(|b| b.is_ascii_digit())
                {
                    let year: i32 = rest[0..4].parse().ok()?;
                    let month: u8 = rest[5..7].parse().ok()?;
                    let day: u8 = rest[8..10].parse().ok()?;
                    let date = time::Date::from_calendar_date(
                        year,
                        time::Month::try_from(month).ok()?,
                        day,
                    )
                    .ok()?;
                    let dt = date.with_hms(0, 0, 0).ok()?;
                    let odt = dt.assume_utc();
                    return Some(odt.unix_timestamp());
                }
            }
        }
        None
    };
    re_like(s)
}

fn head_tree_oid(repo: &Repository) -> Result<ObjectId> {
    let head_oid = refs::resolve_ref(&repo.git_dir, "HEAD")?;
    peel_to_tree(repo, head_oid)
}

fn path_in_tree(repo: &Repository, tree_oid: ObjectId, path: &str) -> bool {
    resolve_tree_path(repo, &tree_oid, path).is_ok()
}

fn path_in_index(repo: &Repository, path: &str, stage: u8) -> bool {
    resolve_index_path_at_stage(repo, path, stage).is_ok()
}

fn diagnose_tree_path_error(
    repo: &Repository,
    rev_label: &str,
    raw_after_colon: &str,
    clean_path: &str,
    err: Error,
) -> Error {
    let Error::ObjectNotFound(msg) = err else {
        return err;
    };
    if !msg.contains("not found in tree") {
        return Error::ObjectNotFound(msg);
    }
    let rel_display: &str =
        if raw_after_colon.starts_with("./") || raw_after_colon.starts_with("../") {
            clean_path
        } else {
            raw_after_colon
        };
    if let Ok(head_tree) = head_tree_oid(repo) {
        if path_in_tree(repo, head_tree, clean_path) {
            return RevParseError::PathOnDiskNotInRevision {
                path: rel_display.to_owned(),
                revision: rev_label.to_owned(),
            }
            .into();
        }
        if let Ok(cwd) = std::env::current_dir() {
            let prefix = show_prefix(repo, &cwd);
            let pfx = prefix.trim_end_matches('/');
            if !pfx.is_empty() {
                let candidate = if clean_path.is_empty() {
                    pfx.to_owned()
                } else {
                    format!("{pfx}/{clean_path}")
                };
                if path_in_tree(repo, head_tree, &candidate) {
                    let alternate_spec = Some(format!("{rev_label}:{candidate}"));
                    return RevParseError::PathPrefixExists {
                        candidate,
                        path: rel_display.to_owned(),
                        alternate_spec,
                    }
                    .into();
                }
            }
        }
        let on_disk = repo
            .work_tree
            .as_ref()
            .map(|wt| wt.join(clean_path))
            .is_some_and(|p| p.exists());
        let in_index = path_in_index(repo, clean_path, 0);
        if on_disk || in_index {
            return RevParseError::PathOnDiskNotInRevision {
                path: rel_display.to_owned(),
                revision: rev_label.to_owned(),
            }
            .into();
        }
    }
    RevParseError::PathNotInRevision {
        path: rel_display.to_owned(),
        revision: rev_label.to_owned(),
    }
    .into()
}

fn diagnose_index_path_error(repo: &Repository, path: &str, stage: u8, err: Error) -> Error {
    let Error::ObjectNotFound(_) = err else {
        return err;
    };
    let work_path = repo
        .work_tree
        .as_ref()
        .map(|wt| wt.join(path))
        .filter(|p| p.exists());
    let on_disk = work_path.is_some();
    let in_head = head_tree_oid(repo)
        .map(|t| path_in_tree(repo, t, path))
        .unwrap_or(false);
    let in_index = path_in_index(repo, path, 0);
    let at_stage = path_in_index(repo, path, stage);

    if stage > 0 && !in_index {
        if let Ok(cwd) = std::env::current_dir() {
            let prefix = show_prefix(repo, &cwd);
            let pfx = prefix.trim_end_matches('/');
            if !pfx.is_empty() {
                let candidate = if path.is_empty() {
                    pfx.to_owned()
                } else {
                    format!("{pfx}/{path}")
                };
                if path_in_index(repo, &candidate, 0) && !path_in_index(repo, &candidate, stage) {
                    let alternate_spec = Some(format!(":0:{candidate}"));
                    return RevParseError::PathIndexPrefixMismatch {
                        candidate,
                        path: path.to_owned(),
                        alternate_spec,
                    }
                    .into();
                }
            }
        }
        return RevParseError::PathMissingFromWorktreeAndIndex {
            path: path.to_owned(),
        }
        .into();
    }

    if stage > 0 && in_index && !at_stage {
        return RevParseError::PathWrongIndexStage {
            path: path.to_owned(),
            stage,
            alternate_spec: Some(format!(":0:{path}")),
        }
        .into();
    }

    if stage == 0 {
        if !on_disk && !in_index {
            if let Ok(cwd) = std::env::current_dir() {
                let prefix = show_prefix(repo, &cwd);
                let pfx = prefix.trim_end_matches('/');
                if !pfx.is_empty() {
                    let candidate = if path.is_empty() {
                        pfx.to_owned()
                    } else {
                        format!("{pfx}/{path}")
                    };
                    if path_in_index(repo, &candidate, 0) {
                        let alternate_spec = Some(format!(":0:{candidate}"));
                        return RevParseError::PathIndexPrefixMismatch {
                            candidate,
                            path: path.to_owned(),
                            alternate_spec,
                        }
                        .into();
                    }
                }
            }
            return RevParseError::PathMissingFromWorktreeAndIndex {
                path: path.to_owned(),
            }
            .into();
        }
        if on_disk && !in_index && !in_head {
            return RevParseError::PathOnDiskNotInIndex {
                path: path.to_owned(),
            }
            .into();
        }
    }
    RevParseError::PathNotInIndex {
        path: path.to_owned(),
    }
    .into()
}

/// Look up a path in the index (stage 0) and return its OID.
fn resolve_index_path(repo: &Repository, path: &str) -> Result<ObjectId> {
    resolve_index_path_at_stage(repo, path, 0)
}

/// Parsed `:path` / `:N:path` index revision syntax (leading colon, not `:/search`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexColonSpec<'a> {
    /// Merge stage (`0` for normal entries, `1`–`3` for unmerged stages).
    pub stage: u8,
    /// Path segment before normalization against the work tree.
    pub raw_path: &'a str,
}

/// If `spec` uses Git's index-only revision form (`:file`, `:0:file`, …), returns the stage and path segment.
///
/// Returns [`None`] for non-index forms such as `HEAD:file`, bare OIDs, or `:/message` search.
#[must_use]
pub fn parse_index_colon_spec(spec: &str) -> Option<IndexColonSpec<'_>> {
    if !spec.starts_with(':') || spec.starts_with(":/") || spec.len() <= 1 {
        return None;
    }
    let rest = &spec[1..];
    if rest.is_empty() {
        return None;
    }
    if rest.len() >= 3 && rest.as_bytes()[1] == b':' {
        if let Some(stage_char) = rest.chars().next() {
            if let Some(stage) = stage_char.to_digit(10) {
                if stage <= 3 {
                    return Some(IndexColonSpec {
                        stage: stage as u8,
                        raw_path: &rest[2..],
                    });
                }
            }
        }
    }
    Some(IndexColonSpec {
        stage: 0,
        raw_path: rest,
    })
}

/// One index entry resolved from a `:path` / `:N:path` revision string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPathEntry {
    /// Repository-relative path using `/` separators (normalized from the spec).
    pub path: String,
    /// Blob OID stored for this index entry.
    pub oid: ObjectId,
    /// Index entry mode (e.g. `0o100644`).
    pub mode: u32,
}

/// Resolve an index revision string (`:file` or `:N:file`) to the staged entry's path, OID, and mode.
///
/// # Returns
///
/// - `Ok(None)` if `spec` is not `:path` index syntax.
/// - `Ok(Some(entry))` on success.
/// - `Err` if the syntax matches but the path is invalid or missing from the index.
pub fn resolve_index_path_entry(repo: &Repository, spec: &str) -> Result<Option<IndexPathEntry>> {
    let Some(colon) = parse_index_colon_spec(spec) else {
        return Ok(None);
    };
    let path = match normalize_colon_path_for_tree(repo, colon.raw_path) {
        Ok(p) => p,
        Err(Error::InvalidRef(msg)) if msg == "outside repository" => {
            let wt = repo
                .work_tree
                .as_ref()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            return Err(RevParseError::OutsideRepository {
                path: colon.raw_path.to_owned(),
                work_tree: wt,
            }
            .into());
        }
        Err(e) => return Err(e),
    };
    let index_path = if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let p = std::path::PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(p)
        } else {
            p
        }
    } else {
        repo.index_path()
    };
    use crate::index::Index;
    let index = Index::load_expand_sparse(&index_path, &repo.odb)
        .map_err(|_| Error::ObjectNotFound(format!(":{}:{}", colon.stage, path)))?;
    let entry = index
        .get(path.as_bytes(), colon.stage)
        .ok_or_else(|| Error::ObjectNotFound(format!(":{}:{}", colon.stage, path)))?;
    Ok(Some(IndexPathEntry {
        path,
        oid: entry.oid,
        mode: entry.mode,
    }))
}

/// Look up a path in the index at a given stage and return its OID.
fn resolve_index_path_at_stage(repo: &Repository, path: &str, stage: u8) -> Result<ObjectId> {
    use crate::index::Index;
    let index_path = if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let p = std::path::PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(p)
        } else {
            p
        }
    } else {
        repo.index_path()
    };
    let index = Index::load_expand_sparse(&index_path, &repo.odb)
        .map_err(|_| Error::ObjectNotFound(format!(":{stage}:{path}")))?;
    match index.get(path.as_bytes(), stage) {
        Some(entry) => Ok(entry.oid),
        None => Err(Error::ObjectNotFound(format!(":{stage}:{path}"))),
    }
}

/// Split `treeish:path` at the first colon that separates a revision from a path,
/// ignoring colons inside `^{...}` peel operators.
///
/// Returns [`None`] for index-only forms like `:path` and `:N:path` (leading `:`).
pub fn split_treeish_colon(spec: &str) -> Option<(&str, &str)> {
    if spec.starts_with(':') {
        return None;
    }
    let bytes = spec.as_bytes();
    let mut i = 0usize;
    let mut peel_depth = 0usize;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'^' && bytes[i + 1] == b'{' {
            peel_depth += 1;
            i += 2;
            continue;
        }
        if peel_depth > 0 {
            if bytes[i] == b'}' {
                peel_depth -= 1;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b':' && i > 0 {
            let before = &spec[..i];
            let after = &spec[i + 1..];
            if !before.is_empty() {
                return Some((before, after)); // after may be empty ("HEAD:" = root tree)
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn split_treeish_spec(spec: &str) -> Option<(&str, &str)> {
    split_treeish_colon(spec)
}

/// Resolve `treeish:path` to the object at `path` (blob, tree, or gitlink OID at the leaf).
///
/// Unlike [`walk_tree_to_blob_entry`], the final path component may name a tree (Git `rev-parse`).
pub(crate) fn resolve_treeish_path_to_object(
    repo: &Repository,
    treeish: ObjectId,
    path: &str,
) -> Result<ObjectId> {
    let object = repo.read_replaced(&treeish)?;
    let mut current_tree = match object.kind {
        ObjectKind::Commit => parse_commit(&object.data)?.tree,
        ObjectKind::Tree => treeish,
        _ => {
            return Err(Error::InvalidRef(format!(
                "object {treeish} does not name a tree"
            )))
        }
    };

    let parts_vec: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts_vec.is_empty() {
        return Ok(current_tree);
    }
    for (idx, part) in parts_vec.iter().enumerate() {
        let tree_object = repo.read_replaced(&current_tree)?;
        if tree_object.kind != ObjectKind::Tree {
            return Err(Error::CorruptObject(format!(
                "object {current_tree} is not a tree"
            )));
        }
        let entries = parse_tree(&tree_object.data)?;
        let Some(entry) = entries.iter().find(|entry| entry.name == part.as_bytes()) else {
            return Err(Error::ObjectNotFound(path.to_owned()));
        };
        if idx + 1 == parts_vec.len() {
            return Ok(entry.oid);
        }
        if entry.mode != crate::index::MODE_TREE {
            return Err(Error::ObjectNotFound(path.to_owned()));
        }
        current_tree = entry.oid;
    }

    Err(Error::ObjectNotFound(path.to_owned()))
}

pub(crate) fn resolve_treeish_path(
    repo: &Repository,
    treeish: ObjectId,
    path: &str,
) -> Result<ObjectId> {
    resolve_treeish_path_to_object(repo, treeish, path)
}

fn apply_peel(repo: &Repository, mut oid: ObjectId, peel: Option<&str>) -> Result<ObjectId> {
    match peel {
        None => Ok(oid),
        Some(search) if search.starts_with('/') => {
            let pattern = &search[1..];
            if pattern.is_empty() {
                return Err(Error::InvalidRef(
                    "empty commit message search pattern".to_owned(),
                ));
            }
            resolve_commit_message_search_from(repo, oid, pattern)
        }
        Some("") => {
            while let Ok(obj) = repo.read_replaced(&oid) {
                if obj.kind != ObjectKind::Tag {
                    break;
                }
                oid = parse_tag_target(&obj.data)?;
            }
            Ok(oid)
        }
        Some("commit") => {
            oid = apply_peel(repo, oid, Some(""))?;
            let obj = repo.read_replaced(&oid)?;
            if obj.kind == ObjectKind::Commit {
                Ok(oid)
            } else {
                Err(Error::InvalidRef("expected commit".to_owned()))
            }
        }
        Some("tree") => {
            // Peel tags, then dereference a commit to its tree.
            oid = apply_peel(repo, oid, Some(""))?;
            let obj = repo.read_replaced(&oid)?;
            match obj.kind {
                ObjectKind::Tree => Ok(oid),
                ObjectKind::Commit => Ok(parse_commit(&obj.data)?.tree),
                _ => Err(Error::InvalidRef("expected tree or commit".to_owned())),
            }
        }
        Some("blob") => {
            // ^{blob}: peel tags until we reach a blob
            let mut cur = oid;
            loop {
                let obj = repo.read_replaced(&cur)?;
                match obj.kind {
                    ObjectKind::Blob => return Ok(cur),
                    ObjectKind::Tag => {
                        cur = parse_tag_target(&obj.data)?;
                    }
                    _ => return Err(Error::InvalidRef("expected blob".to_owned())),
                }
            }
        }
        Some("object") => Ok(oid),
        Some("tag") => {
            // ^{tag}: return if it's a tag object
            let obj = repo.read_replaced(&oid)?;
            if obj.kind == ObjectKind::Tag {
                Ok(oid)
            } else {
                Err(Error::InvalidRef("expected tag".to_owned()))
            }
        }
        Some(other) => Err(Error::InvalidRef(format!(
            "unsupported peel operator '{{{other}}}'"
        ))),
    }
}

/// Expand a single revision token that ends with `^!` (Git: commit without its parents).
///
/// Returns one token unchanged when `^!` is absent. When present, returns the base revision
/// (without `^!`) plus one `^<parent-hex>` entry per parent from [`commit_parents_for_navigation`]
/// (commit object parents plus graft/replace overrides), matching Git’s `^!` expansion for
/// merge commits.
///
/// # Errors
///
/// Returns [`RevParseError::AmbiguousArgument`] for an empty base revision and other resolution failures.
pub fn expand_rev_token_circ_bang(repo: &Repository, token: &str) -> Result<Vec<String>> {
    let Some(base) = token.strip_suffix("^!") else {
        return Ok(vec![token.to_owned()]);
    };
    if base.is_empty() {
        return Err(RevParseError::AmbiguousArgument {
            spec: token.to_owned(),
        }
        .into());
    }
    let oid = resolve_revision_for_range_end(repo, base)?;
    let commit_oid = peel_to_commit_for_merge_base(repo, oid)?;
    let parents = commit_parents_for_navigation(repo, commit_oid)?;
    let mut out = vec![base.to_owned()];
    for p in parents {
        out.push(format!("^{}", p.to_hex()));
    }
    Ok(out)
}

/// Split `spec` into `(base, peel_inner)` for `^{...}` / `^0` suffixes (same rules as revision parsing).
#[must_use]
pub fn parse_peel_suffix(spec: &str) -> (&str, Option<&str>) {
    if let Some(base) = spec.strip_suffix("^{}") {
        return (base, Some(""));
    }
    if let Some(start) = spec.rfind("^{") {
        if spec.ends_with('}') {
            let base = &spec[..start];
            let op = &spec[start + 2..spec.len() - 1];
            return (base, Some(op));
        }
    }
    // `^0` is shorthand for `^{commit}` — peel tags and verify commit.
    if let Some(base) = spec.strip_suffix("^0") {
        // Only match if the character before `^0` is not also a `^` (avoid
        // matching `^^0` as a peel instead of nav+nav).
        if !base.ends_with('^') {
            return (base, Some("commit"));
        }
    }
    (spec, None)
}

fn parse_tag_target(data: &[u8]) -> Result<ObjectId> {
    let text = std::str::from_utf8(data)
        .map_err(|_| Error::CorruptObject("invalid tag object".to_owned()))?;
    let Some(line) = text.lines().find(|line| line.starts_with("object ")) else {
        return Err(Error::CorruptObject("tag missing object header".to_owned()));
    };
    let oid_text = line.trim_start_matches("object ").trim();
    oid_text.parse::<ObjectId>()
}

/// Search commit messages reachable from `start` and return the first commit
/// whose message contains `pattern`.
/// Parse the leading `!` semantics of a `^{/<pattern>}` (or `:/<pattern>`)
/// commit-message search, matching git's `get_oid_oneline` in `object-name.c`.
///
/// Returns `(negate, effective_pattern)` where `effective_pattern` is the
/// regular expression to compile and `negate` flips the match sense:
///   - `!-<pat>` is a negated search (find a commit NOT matching `<pat>`).
///   - `!!<pat>` escapes to a literal `!<pat>` regex search.
///   - any other `!` prefix (e.g. `!Exp`, `!`, `!-` is fine but `!x` not) is
///     reserved and yields `None`, signalling the caller to fail.
fn parse_oneline_pattern(pattern: &str) -> Option<(bool, &str)> {
    let Some(after_bang) = pattern.strip_prefix('!') else {
        return Some((false, pattern));
    };
    if let Some(neg) = after_bang.strip_prefix('-') {
        return Some((true, neg));
    }
    if after_bang.starts_with('!') {
        // `!!<pat>` -> literal `!<pat>`; keep one leading `!` for the regex.
        return Some((false, after_bang));
    }
    // Reserved: `!` followed by anything other than `!` or `-`.
    None
}

fn resolve_commit_message_search_from(
    repo: &Repository,
    start: ObjectId,
    pattern: &str,
) -> Result<ObjectId> {
    let Some((negate, effective_pattern)) = parse_oneline_pattern(pattern) else {
        return Err(Error::ObjectNotFound(format!(":/{pattern}")));
    };
    let regex = Regex::new(effective_pattern).ok();
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(start);
    visited.insert(start);

    while let Some(oid) = queue.pop_front() {
        let obj = match repo.read_replaced(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = match parse_commit(&obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let base_match = if let Some(re) = &regex {
            re.is_match(&commit.message)
        } else {
            commit.message.contains(effective_pattern)
        };
        let is_match = negate ^ base_match;
        if is_match {
            return Ok(oid);
        }

        for parent in &commit.parents {
            if visited.insert(*parent) {
                queue.push_back(*parent);
            }
        }
    }

    Err(Error::ObjectNotFound(format!(":/{pattern}")))
}

fn find_abbrev_matches(repo: &Repository, prefix: &str) -> Result<Vec<ObjectId>> {
    let max_len = repo.odb.hash_algo().hex_len();
    if !is_hex_prefix(prefix) || !(4..=max_len).contains(&prefix.len()) {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for objects_dir in object_storage_dirs_for_abbrev(repo)? {
        for hex in collect_loose_object_ids_in_dir(&objects_dir)? {
            if hex.starts_with(prefix) {
                let oid = hex.parse::<ObjectId>()?;
                if seen.insert(oid) {
                    matches.push(oid);
                }
            }
        }
        for oid in collect_pack_oids_with_prefix(&objects_dir, prefix)? {
            if seen.insert(oid) {
                matches.push(oid);
            }
        }
    }
    Ok(matches)
}

fn collect_loose_object_ids(repo: &Repository) -> Result<Vec<String>> {
    collect_loose_object_ids_in_dir(repo.odb.objects_dir())
}

fn collect_loose_object_ids_in_dir(objects_dir: &Path) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let read = match fs::read_dir(objects_dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
        Err(err) => return Err(Error::Io(err)),
    };

    for dir_entry in read {
        let dir_entry = dir_entry?;
        let name = dir_entry.file_name();
        let Some(prefix) = name.to_str() else {
            continue;
        };
        if !is_two_hex(prefix) {
            continue;
        }
        if !dir_entry.file_type()?.is_dir() {
            continue;
        }

        let files = fs::read_dir(dir_entry.path())?;
        for file_entry in files {
            let file_entry = file_entry?;
            if !file_entry.file_type()?.is_file() {
                continue;
            }
            let file_name = file_entry.file_name();
            let Some(suffix) = file_name.to_str() else {
                continue;
            };
            if ObjectId::is_loose_suffix_len(suffix.len())
                && suffix.chars().all(|ch| ch.is_ascii_hexdigit())
            {
                ids.push(format!("{prefix}{suffix}"));
            }
        }
    }

    Ok(ids)
}

fn is_two_hex(text: &str) -> bool {
    text.len() == 2 && text.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn is_hex_prefix(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn path_is_within(path: &Path, container: &Path) -> bool {
    if path == container {
        return true;
    }
    path.starts_with(container)
}

fn normalize_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::RootDir => Some(String::from("/")),
            Component::Normal(item) => Some(item.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn component_to_text(component: Component<'_>) -> Option<String> {
    match component {
        Component::Normal(item) => Some(os_to_string(item)),
        _ => None,
    }
}

fn os_to_string(text: &OsStr) -> String {
    text.to_string_lossy().into_owned()
}

/// Search commit messages from HEAD backwards for a commit whose message
/// contains `pattern`.  Returns the first matching commit OID.
fn resolve_commit_message_search(
    repo: &crate::repo::Repository,
    pattern: &str,
) -> Result<ObjectId> {
    // Handle negated pattern: /! means negate; /!! means literal /!
    let (negate, effective_pattern) = if let Some(rest) = pattern.strip_prefix('!') {
        if rest.starts_with('!') {
            (false, rest) // !! = literal leading !
        } else {
            (true, rest) // ! = negate
        }
    } else {
        (false, pattern)
    };
    let regex = Regex::new(effective_pattern).ok();
    use crate::state::resolve_head;
    let head =
        resolve_head(&repo.git_dir).map_err(|_| Error::ObjectNotFound(format!(":/{pattern}")))?;
    let start_oid = match head.oid() {
        Some(oid) => *oid,
        None => return Err(Error::ObjectNotFound(format!(":/{pattern}"))),
    };

    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(start_oid);
    visited.insert(start_oid);
    if let Ok(refs) = crate::refs::list_refs(&repo.git_dir, "refs/") {
        for (_name, oid) in refs {
            if visited.insert(oid) {
                queue.push_back(oid);
            }
        }
    }

    while let Some(oid) = queue.pop_front() {
        let obj = match repo.read_replaced(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        // Skip non-commit objects
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = match parse_commit(&obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check if message matches pattern (regex, with literal fallback)
        let base_match = if let Some(re) = &regex {
            re.is_match(&commit.message)
        } else {
            commit.message.contains(effective_pattern)
        };
        let is_match = if negate { !base_match } else { base_match };
        if is_match {
            return Ok(oid);
        }

        // Enqueue parents
        for parent in &commit.parents {
            if visited.insert(*parent) {
                queue.push_back(*parent);
            }
        }
    }

    Err(Error::ObjectNotFound(format!(":/{pattern}")))
}

/// All object IDs (loose and packed) whose hex form starts with `prefix`.
pub fn list_all_abbrev_matches(repo: &Repository, prefix: &str) -> Result<Vec<ObjectId>> {
    find_abbrev_matches(repo, prefix)
}

/// Public: find all object IDs whose hex prefix matches the given string.
pub fn list_loose_abbrev_matches(repo: &Repository, prefix: &str) -> Result<Vec<ObjectId>> {
    list_all_abbrev_matches(repo, prefix)
}

#[cfg(test)]
mod superproject_path_tests {
    use super::superproject_work_tree_from_nested_git_modules;
    use std::path::PathBuf;

    #[test]
    fn nested_modules_yields_superproject_work_tree() {
        let git_dir = PathBuf::from("/tmp/super/.git/modules/dir/modules/sub");
        assert_eq!(
            superproject_work_tree_from_nested_git_modules(&git_dir),
            Some(PathBuf::from("/tmp/super"))
        );
    }

    #[test]
    fn non_nested_returns_none() {
        let git_dir = PathBuf::from("/tmp/repo/.git");
        assert!(superproject_work_tree_from_nested_git_modules(&git_dir).is_none());
    }
}
