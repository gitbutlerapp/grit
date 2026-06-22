//! Submodule recursion for `git push` (`--recurse-submodules`).
//!
//! Mirrors the subset of Git's `submodule.c` / `transport.c` logic needed for
//! `check`, `on-demand`, and `only` modes over local (file) transport.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::combined_tree_diff::{combined_diff_paths_filtered, CombinedTreeDiffOptions};
use crate::diff::{diff_trees, DiffStatus};
use crate::error::Result;
use crate::index::MODE_GITLINK;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::refs;
use crate::repo::Repository;

fn resolve_remote_url_to_local_git_dir(url: &str, base_for_relative: &Path) -> Option<PathBuf> {
    let url = url.trim();
    if url.starts_with("git://")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || is_ssh_transport_url(url)
    {
        return None;
    }
    let path_str = url.strip_prefix("file://").unwrap_or(url);
    let mut p = PathBuf::from(path_str);
    if p.is_relative() {
        p = base_for_relative.join(p);
    }
    let p = if p.ends_with(".git") || p.join("HEAD").exists() {
        p
    } else {
        p.join(".git")
    };
    if p.join("HEAD").exists() {
        Some(p)
    } else {
        None
    }
}

fn is_ssh_transport_url(url: &str) -> bool {
    if url.starts_with("ssh://") || url.starts_with("git+ssh://") {
        return true;
    }
    if url.contains("://") {
        return false;
    }
    let colon = url.find(':');
    let slash = url.find('/');
    colon.is_some_and(|ci| slash.is_none_or(|si| ci < si))
}

/// True when `rev-list <oids> --not <remote_tip_oids>` is non-empty: some gitlink commit is not on the remote.
fn oids_not_on_remote_repo(
    submodule_repo: &Repository,
    oids: &[ObjectId],
    remote_git_dir: &Path,
) -> Result<bool> {
    if oids.is_empty() {
        return Ok(false);
    }
    let remote_heads = refs::list_refs(remote_git_dir, "refs/heads/")?;
    let negative: Vec<String> = remote_heads.iter().map(|(_, o)| o.to_hex()).collect();
    let positive: Vec<String> = oids.iter().map(|o| o.to_hex()).collect();
    let options = RevListOptions::default();
    let r = rev_list(submodule_repo, &positive, &negative, &options)?;
    Ok(!r.commits.is_empty())
}
use crate::rev_list::{rev_list, RevListOptions};
use crate::state::{resolve_head, HeadState};

/// How `git push` should recurse into submodules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRecurseSubmodules {
    /// No submodule handling (default when unset).
    Off,
    /// Verify gitlink targets exist on a submodule remote (`check`).
    Check,
    /// Push submodule repos as needed (`on-demand`).
    OnDemand,
    /// Push only submodules, not the superproject (`only`).
    Only,
}

/// Parse `--recurse-submodules=<value>` or `push.recurseSubmodules` / `submodule.recurse`.
///
/// Returns an error for invalid values (`yes`, unknown strings, etc.).
pub fn parse_push_recurse_submodules_arg(
    opt: &str,
    arg: &str,
) -> std::result::Result<PushRecurseSubmodules, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(format!("option `{opt}` requires a value"));
    }

    // Internal sentinel used when Git recurses from `only` into a child push.
    if arg == "only-is-on-demand" {
        return Ok(PushRecurseSubmodules::OnDemand);
    }

    match crate::config::parse_bool(arg) {
        Ok(true) => Err(format!("bad {opt} argument: {arg}")),
        Ok(false) => Ok(PushRecurseSubmodules::Off),
        Err(_) => {
            if arg.eq_ignore_ascii_case("on-demand") {
                Ok(PushRecurseSubmodules::OnDemand)
            } else if arg.eq_ignore_ascii_case("check") {
                Ok(PushRecurseSubmodules::Check)
            } else if arg.eq_ignore_ascii_case("only") {
                Ok(PushRecurseSubmodules::Only)
            } else if arg.eq_ignore_ascii_case("no") || arg.eq_ignore_ascii_case("false") {
                Ok(PushRecurseSubmodules::Off)
            } else {
                Err(format!("bad {opt} argument: {arg}"))
            }
        }
    }
}

fn mode_from_octal(mode_str: &str) -> Option<u32> {
    u32::from_str_radix(mode_str, 8).ok()
}

fn is_gitlink_mode(mode_str: &str) -> bool {
    mode_from_octal(mode_str) == Some(MODE_GITLINK)
}

/// Collect submodule paths and the gitlink OIDs introduced along the walk
/// `git log <tips> --not --remotes=<remote>`, using merge-aware diffs like Git's
/// `collect_changed_submodules`.
///
/// The negative side is the superproject's `refs/remotes/<remote>/*` tracking refs, exactly like
/// Git's `find_unpushed_submodules` (`--not --remotes=<name>`). When pushing by URL (or before any
/// fetch) there are no such tracking refs, so the walk covers the full reachable history and the
/// per-submodule check ([`submodule_needs_push_to_remote`]) is responsible for excluding gitlink
/// commits that already exist on the submodule's own remote. We deliberately do **not** prune the
/// superproject walk by the destination repository's tips: doing so would skip submodule pushes
/// when the superproject ref is already up to date on the remote but the submodule commit is not
/// (e.g. `git push --recurse-submodules=on-demand` after a prior `--no-recurse-submodules` push).
pub fn collect_changed_gitlinks_for_push(
    repo: &Repository,
    commit_tips: &[ObjectId],
    exclude_remote_name: &str,
    _fallback_remote_git_dir: Option<&Path>,
) -> Result<HashMap<String, Vec<ObjectId>>> {
    if commit_tips.is_empty() {
        return Ok(HashMap::new());
    }

    let prefix = format!("refs/remotes/{exclude_remote_name}/");
    let remote_refs = refs::list_refs(&repo.git_dir, &prefix)?;
    let negative_hex: Vec<String> = remote_refs.iter().map(|(_, oid)| oid.to_hex()).collect();

    let positive_hex: Vec<String> = commit_tips.iter().map(|o| o.to_hex()).collect();
    let options = RevListOptions::default();
    let walked = rev_list(repo, &positive_hex, &negative_hex, &options)?;

    let odb = &repo.odb;
    let walk_opts = CombinedTreeDiffOptions {
        recursive: true,
        tree_in_recursive: false,
    };

    let mut by_path: HashMap<String, Vec<ObjectId>> = HashMap::new();

    for commit_oid in walked.commits {
        let obj = odb.read(&commit_oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        let parents = commit.parents;

        if parents.is_empty() {
            let entries = diff_trees(odb, None, Some(&commit.tree), "")?;
            for e in entries {
                if !is_gitlink_mode(&e.new_mode) {
                    continue;
                }
                let path = e.path().to_string_lossy().into_owned();
                by_path.entry(path).or_default().push(e.new_oid);
            }
        } else if parents.len() == 1 {
            let pobj = odb.read(&parents[0])?;
            if pobj.kind != ObjectKind::Commit {
                continue;
            }
            let parent = parse_commit(&pobj.data)?;
            let entries = diff_trees(odb, Some(&parent.tree), Some(&commit.tree), "")?;
            for e in entries {
                if !matches!(
                    e.status,
                    DiffStatus::Added
                        | DiffStatus::Modified
                        | DiffStatus::TypeChanged
                        | DiffStatus::Renamed
                ) {
                    continue;
                }
                let (mode, oid) = match e.status {
                    DiffStatus::Deleted => continue,
                    _ => (&e.new_mode, e.new_oid),
                };
                if !is_gitlink_mode(mode) {
                    continue;
                }
                let path = e
                    .new_path
                    .as_deref()
                    .or(e.old_path.as_deref())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if path.is_empty() {
                    continue;
                }
                by_path.entry(path).or_default().push(oid);
            }
        } else {
            let paths =
                combined_diff_paths_filtered(odb, &commit.tree, &parents, &walk_opts, None)?;
            for p in paths {
                if (p.merge_mode & 0o170000) != MODE_GITLINK {
                    continue;
                }
                if p.merge_oid.is_zero() {
                    continue;
                }
                by_path.entry(p.path).or_default().push(p.merge_oid);
            }
        }
    }

    for v in by_path.values_mut() {
        v.sort();
        v.dedup();
    }

    Ok(by_path)
}

/// True when walking superproject commits in `(excl..incl]` introduces or changes a submodule
/// gitlink (matches Git's `submodule_touches_in_range` in `submodule.c`).
///
/// Used by `git pull --rebase --recurse-submodules` to reject rebases when local commits already
/// recorded submodule pointer changes.
pub fn submodule_gitlinks_touched_in_range(
    repo: &Repository,
    excl: Option<ObjectId>,
    incl: ObjectId,
) -> Result<bool> {
    let positive = vec![incl.to_hex()];
    let negative = excl.map(|e| vec![e.to_hex()]).unwrap_or_default();
    let options = RevListOptions::default();
    let walked = rev_list(repo, &positive, &negative, &options)?;
    let odb = &repo.odb;
    let walk_opts = CombinedTreeDiffOptions {
        recursive: true,
        tree_in_recursive: false,
    };

    for commit_oid in walked.commits {
        let obj = odb.read(&commit_oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        let parents = commit.parents;

        if parents.is_empty() {
            // Root commits: Git's `submodule_touches_in_range` / combined-diff skips these (no
            // parents → combined diff returns early). Do not treat "added submodule at init" as a
            // local submodule modification for `pull --rebase` (t5572).
            continue;
        } else if parents.len() == 1 {
            let pobj = odb.read(&parents[0])?;
            if pobj.kind != ObjectKind::Commit {
                continue;
            }
            let parent = parse_commit(&pobj.data)?;
            let entries = diff_trees(odb, Some(&parent.tree), Some(&commit.tree), "")?;
            for e in entries {
                if !matches!(
                    e.status,
                    DiffStatus::Added
                        | DiffStatus::Modified
                        | DiffStatus::TypeChanged
                        | DiffStatus::Renamed
                ) {
                    continue;
                }
                let mode = match e.status {
                    DiffStatus::Deleted => continue,
                    _ => &e.new_mode,
                };
                if is_gitlink_mode(mode) {
                    return Ok(true);
                }
            }
        } else {
            let paths =
                combined_diff_paths_filtered(odb, &commit.tree, &parents, &walk_opts, None)?;
            for p in paths {
                if (p.merge_mode & 0o170000) == MODE_GITLINK && !p.merge_oid.is_zero() {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

/// Work tree path for a submodule at `rel_path` in the superproject.
pub fn submodule_worktree_path(super_repo: &Repository, rel_path: &str) -> PathBuf {
    super_repo
        .work_tree
        .as_ref()
        .map(|wt| wt.join(rel_path))
        .unwrap_or_else(|| super_repo.git_dir.join(rel_path))
}

/// True if `path` under the superproject looks like a checked-out nested repo (has `.git`).
fn submodule_populated_at(super_repo: &Repository, rel_path: &str) -> bool {
    let wd = submodule_worktree_path(super_repo, rel_path);
    wd.join(".git").exists()
}

/// Whether the gitlink OIDs are commits present in the submodule repo and reachable from some ref.
pub fn submodule_commits_fully_pushed(
    super_repo: &Repository,
    rel_path: &str,
    oids: &[ObjectId],
) -> Result<bool> {
    if oids.is_empty() {
        return Ok(true);
    }

    let wd = submodule_worktree_path(super_repo, rel_path);
    if !wd.join(".git").exists() {
        // Without a checkout Git skips the strict check (expert path).
        return Ok(true);
    }

    let sub = Repository::discover(Some(&wd))?;
    let odb = &sub.odb;

    for oid in oids {
        let obj = match odb.read(oid) {
            Ok(o) => o,
            Err(_) => return Ok(false),
        };
        match obj.kind {
            ObjectKind::Commit => {}
            ObjectKind::Tag => {
                return Err(crate::error::Error::Message(format!(
                    "submodule entry '{rel_path}' ({}) is a tag, not a commit",
                    oid.to_hex()
                )));
            }
            other => {
                return Err(crate::error::Error::Message(format!(
                    "submodule entry '{rel_path}' ({}) is a {other:?}, not a commit",
                    oid.to_hex()
                )));
            }
        }
    }

    let all_refs = refs::list_refs(&sub.git_dir, "refs/")?;
    let negative: Vec<String> = all_refs.iter().map(|(_, o)| o.to_hex()).collect();
    let positive: Vec<String> = oids.iter().map(|o| o.to_hex()).collect();
    let options = RevListOptions::default();
    let r = rev_list(&sub, &positive, &negative, &options)?;
    Ok(r.commits.is_empty())
}

/// True if `oids` contains commits not reachable from `refs/remotes/<remote_name>/` in the submodule.
pub fn submodule_needs_push_to_remote(
    super_repo: &Repository,
    rel_path: &str,
    _remote_name: &str,
    oids: &[ObjectId],
) -> Result<bool> {
    if oids.is_empty() {
        return Ok(false);
    }

    if !submodule_populated_at(super_repo, rel_path) {
        return Ok(false);
    }

    let wd = submodule_worktree_path(super_repo, rel_path);
    let sub = Repository::discover(Some(&wd))?;

    for oid in oids {
        let obj = match sub.odb.read(oid) {
            Ok(o) => o,
            Err(_) => return Ok(false),
        };
        if obj.kind != ObjectKind::Commit {
            return Ok(false);
        }
    }

    // Match Git's `submodule_needs_pushing`: `rev-list <oids> --not --remotes` uses **all**
    // submodule remote-tracking refs, not the superproject's remote name (which may be a URL).
    let all_remote_tracking = refs::list_refs(&sub.git_dir, "refs/remotes/")?;
    if !all_remote_tracking.is_empty() {
        let negative: Vec<String> = all_remote_tracking
            .iter()
            .map(|(_, o)| o.to_hex())
            .collect();
        let positive: Vec<String> = oids.iter().map(|o| o.to_hex()).collect();
        let options = RevListOptions::default();
        let r = rev_list(&sub, &positive, &negative, &options)?;
        return Ok(!r.commits.is_empty());
    }

    // No remote-tracking refs (e.g. `grit fetch` did not update `refs/remotes/*`): probe each
    // configured `remote.*.url` that resolves to a local repo, like a one-sided `--remotes`.
    let cfg = crate::config::ConfigSet::load(Some(&sub.git_dir), true).unwrap_or_default();
    let mut saw_url = false;
    for entry in cfg.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some((_remote, key)) = rest.split_once('.') else {
            continue;
        };
        if key != "url" {
            continue;
        }
        saw_url = true;
        let Some(val) = entry.value.as_deref() else {
            continue;
        };
        let Some(remote_git_dir) = resolve_remote_url_to_local_git_dir(val, &wd) else {
            continue;
        };
        if oids_not_on_remote_repo(&sub, oids, &remote_git_dir)? {
            return Ok(true);
        }
    }
    if !saw_url {
        return Ok(false);
    }
    Ok(false)
}

/// Ensure every gitlink OID in `changed` names a commit object (not a tag/tree/blob).
///
/// Tries the superproject ODB first, then the checked-out submodule's ODB (embedded repos keep
/// submodule objects outside the superproject object store).
pub fn verify_push_gitlinks_are_commits(
    repo: &Repository,
    changed: &HashMap<String, Vec<ObjectId>>,
) -> Result<()> {
    for (path, oids) in changed {
        let sub_odb = if submodule_populated_at(repo, path) {
            let wd = submodule_worktree_path(repo, path);
            Repository::discover(Some(&wd)).ok().map(|s| s.odb)
        } else {
            None
        };

        for oid in oids {
            let obj = match repo.odb.read(oid) {
                Ok(o) => o,
                Err(crate::error::Error::ObjectNotFound(_)) => {
                    let Some(ref sodb) = sub_odb else {
                        return Err(crate::error::Error::ObjectNotFound(oid.to_hex()));
                    };
                    sodb.read(oid)?
                }
                Err(e) => return Err(e),
            };
            match obj.kind {
                ObjectKind::Commit => {}
                ObjectKind::Tag => {
                    return Err(crate::error::Error::Message(format!(
                        "submodule entry '{path}' ({}) is a tag, not a commit",
                        oid.to_hex()
                    )));
                }
                other => {
                    return Err(crate::error::Error::Message(format!(
                        "submodule entry '{path}' ({}) is a {other:?}, not a commit",
                        oid.to_hex()
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Submodule paths that still need to be pushed to `remote_name` (non-empty rev-list against remote-tracking).
pub fn find_unpushed_submodule_paths(
    super_repo: &Repository,
    pushed_commit_tips: &[ObjectId],
    remote_name: &str,
    fallback_remote_git_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let changed = collect_changed_gitlinks_for_push(
        super_repo,
        pushed_commit_tips,
        remote_name,
        fallback_remote_git_dir,
    )?;
    let mut needs: Vec<String> = Vec::new();
    for (path, oids) in changed {
        if submodule_needs_push_to_remote(super_repo, &path, remote_name, &oids)? {
            needs.push(path);
        }
    }
    needs.sort();
    needs.dedup();
    Ok(needs)
}

/// Print Git's standard "unpushed submodule" error and return a formatted anyhow-friendly message.
pub fn format_unpushed_submodules_error(paths: &[String]) -> String {
    let mut msg = String::from(
        "The following submodule paths contain changes that can\n\
not be found on any remote:\n",
    );
    for p in paths {
        msg.push_str(&format!("  {p}\n"));
    }
    msg.push_str(
        "\nPlease try\n\n\
\tgit push --recurse-submodules=on-demand\n\n\
or cd to the path and use\n\n\
\tgit push\n\n\
to push them to a remote.\n\n\
Aborting.",
    );
    msg
}

/// Resolve `HEAD` in `git_dir` to a short branch name when symbolic; `"HEAD"` when detached.
pub fn head_ref_short_name(git_dir: &Path) -> Result<String> {
    let head = resolve_head(git_dir)?;
    Ok(match head {
        HeadState::Branch { refname, .. } => refname
            .strip_prefix("refs/heads/")
            .unwrap_or(&refname)
            .to_string(),
        HeadState::Detached { .. } | HeadState::Invalid => "HEAD".to_string(),
    })
}

fn refspec_is_pushable_for_validation(spec: &str) -> bool {
    if spec.starts_with('+') {
        return refspec_is_pushable_for_validation(&spec[1..]);
    }
    if spec == ":" || spec == "+:" {
        return false;
    }
    if spec.contains('*') {
        return false;
    }
    let (src, _) = if let Some(i) = spec.find(':') {
        (&spec[..i], &spec[i + 1..])
    } else {
        (spec, spec)
    };
    !src.is_empty()
}

/// Validate refspecs for nested submodule push (`submodule--helper push-check` subset).
pub fn validate_submodule_push_refspecs(
    submodule_git_dir: &Path,
    superproject_head_branch: &str,
    refspecs: &[String],
) -> Result<()> {
    for spec in refspecs {
        if !refspec_is_pushable_for_validation(spec) {
            continue;
        }
        let (force, rest) = spec
            .strip_prefix('+')
            .map(|s| (true, s))
            .unwrap_or((false, spec.as_str()));
        let (src, _) = if let Some(i) = rest.find(':') {
            (&rest[..i], &rest[i + 1..])
        } else {
            (rest, rest)
        };
        if src.is_empty() {
            continue;
        }

        let sub_head = resolve_head(submodule_git_dir)?;
        let (detached, head_branch) = match &sub_head {
            HeadState::Branch { refname, .. } => (
                false,
                refname
                    .strip_prefix("refs/heads/")
                    .unwrap_or(refname)
                    .to_string(),
            ),
            _ => (true, String::new()),
        };

        let matches = count_src_refspec_matches(submodule_git_dir, src)?;
        match matches {
            1 => {}
            _ => {
                if src == "HEAD" && (detached || head_branch == superproject_head_branch) {
                    // Allowed:
                    // - detached HEAD in the submodule (`HEAD:<dst>` push of current commit)
                    // - symbolic HEAD on the same branch as the superproject.
                    continue;
                }
                return Err(crate::error::Error::Message(format!(
                    "src refspec '{src}' must name a ref"
                )));
            }
        }
        let _ = force;
    }
    Ok(())
}

fn count_src_refspec_matches(git_dir: &Path, src: &str) -> Result<usize> {
    if src.starts_with("refs/") {
        return Ok(usize::from(refs::resolve_ref(git_dir, src).is_ok()));
    }
    if src.len() == 40 && src.parse::<ObjectId>().is_ok() {
        return Ok(1);
    }
    let mut n = 0usize;
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        let full = format!("{prefix}{src}");
        if refs::resolve_ref(git_dir, &full).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}
