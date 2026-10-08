//! Branch vs remote-tracking comparison for status, checkout, and commit (matches `git/remote.c`).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::config::ConfigSet;
use crate::error::Result;
use crate::merge_base::count_symmetric_ahead_behind;
use crate::objects::ObjectId;
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refs;
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::repo::Repository;
use crate::rev_parse::{
    abbreviate_ref_name, resolve_push_full_ref_for_branch, resolve_upstream_symbolic_name,
};
use crate::transfer::{match_positive, ref_excluded};

/// How to compare local HEAD to a remote-tracking ref (`AHEAD_BEHIND_FULL` vs `QUICK`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AheadBehindMode {
    /// Compute exact ahead/behind counts (`rev-list --left-right`).
    Full,
    /// Only detect same vs different (cheap).
    Quick,
}

/// Outcome of comparing `refs/heads/<branch>` to a tracking ref.
#[derive(Clone, Debug)]
pub enum TrackingStat {
    /// Tips are the same commit.
    UpToDate,
    /// Tracking ref is missing (gone upstream).
    Gone {
        /// Short display name for the missing tracking ref.
        display_name: String,
    },
    /// Tips differ; counts are zero in [`AheadBehindMode::Quick`] mode.
    Diverged {
        /// Short display name for the tracking ref.
        display_name: String,
        /// Number of commits local branch is ahead.
        ahead: usize,
        /// Number of commits local branch is behind.
        behind: usize,
    },
}

/// Short display name for a full ref (`refs/remotes/origin/main` -> `origin/main`).
#[must_use]
pub fn shorten_tracking_ref(full_ref: &str) -> String {
    abbreviate_ref_name(full_ref)
}

/// Resolve the local tracking ref for a remote ref using `remote.<name>.fetch`
/// refspecs (Git `remote_find_tracking` / `refspec_find_match` on the fetch list).
///
/// Returns `None` when no positive fetch refspec maps `remote_ref`, or when a
/// negative refspec excludes it. When no fetch refspecs are configured at all,
/// falls back to `refs/remotes/<remote>/<branch>` for `refs/heads/*` only.
#[must_use]
pub fn tracking_ref_for_remote_push_ref(
    git_dir: &Path,
    remote_name: &str,
    remote_ref: &str,
) -> Option<String> {
    if remote_name.contains('/') || remote_name.starts_with('.') {
        return None;
    }

    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(git_dir),
        true,
    )
    .ok()?;
    let (positive, negative) = remote_fetch_refspecs(&config, remote_name);
    if ref_excluded(remote_ref, &negative) {
        return None;
    }
    match match_positive(remote_ref, &positive) {
        Some(Some(dst)) => Some(dst),
        Some(None) => None,
        None if positive.is_empty() => remote_ref
            .strip_prefix("refs/heads/")
            .map(|branch| format!("refs/remotes/{remote_name}/{branch}")),
        None => None,
    }
}

fn remote_fetch_refspecs(
    config: &ConfigSet,
    remote_name: &str,
) -> (Vec<RefspecItem>, Vec<RefspecItem>) {
    let key = format!("remote.{remote_name}.fetch");
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for spec in config.get_all(&key) {
        let Ok(item) = parse_fetch_refspec(&spec) else {
            continue;
        };
        if item.negative {
            negative.push(item);
        } else {
            positive.push(item);
        }
    }
    (positive, negative)
}

/// Update a local remote-tracking ref after a successful push to a named remote.
///
/// The destination ref is resolved through the remote's configured fetch refspecs.
/// Path-like remote names (containing `/` or starting with `.`) skip tracking updates.
///
/// # Parameters
///
/// - `git_dir`: local repository `.git` directory.
/// - `remote_name`: short remote name (e.g. `origin`).
/// - `remote_ref`: destination ref on the remote (e.g. `refs/heads/main`).
/// - `new_oid`: new tip after the push, or `None` when the remote ref was deleted.
///
/// # Errors
///
/// Returns an error if writing or deleting the tracking ref fails.
pub fn update_remote_tracking_ref(
    git_dir: &Path,
    remote_name: &str,
    remote_ref: &str,
    new_oid: Option<ObjectId>,
) -> Result<()> {
    let Some(tracking_ref) = tracking_ref_for_remote_push_ref(git_dir, remote_name, remote_ref)
    else {
        return Ok(());
    };

    match new_oid {
        Some(oid) => {
            if refs::resolve_ref(git_dir, &tracking_ref).ok() == Some(oid) {
                return Ok(());
            }
            refs::write_ref(git_dir, &tracking_ref, &oid)?;
        }
        None => {
            let _ = refs::delete_ref(git_dir, &tracking_ref);
        }
    }
    Ok(())
}

/// Apply remote-tracking ref updates for every successfully pushed ref in `results`.
///
/// Refs reported as [`PushRefStatus::Ok`] or [`PushRefStatus::UpToDate`] are
/// updated (matching Git `transport_update_tracking_ref`). Deletions pass
/// `new_oid: None` to remove the tracking ref.
///
/// # Errors
///
/// Propagates errors from [`update_remote_tracking_ref`].
pub fn apply_push_remote_tracking_updates(
    git_dir: &Path,
    remote_name: &str,
    results: &[PushRefResult],
) -> Result<()> {
    for result in results {
        if result.status != PushRefStatus::Ok && result.status != PushRefStatus::UpToDate {
            continue;
        }
        update_remote_tracking_ref(git_dir, remote_name, &result.remote_ref, result.new_oid)?;
    }
    Ok(())
}

fn branch_head_ref(short_name: &str) -> String {
    format!("refs/heads/{short_name}")
}

/// Full ref for the configured upstream of `branch_short` (`refs/remotes/...` or `refs/heads/...`).
#[must_use]
pub fn upstream_tracking_full_ref(repo: &Repository, branch_short: &str) -> Option<String> {
    let config = repo.config().ok()?;
    let remote = config.get(&format!("branch.{branch_short}.remote"))?;
    let merge = config.get(&format!("branch.{branch_short}.merge"))?;
    if remote == "." {
        let m = merge.trim();
        if m.starts_with("refs/") {
            Some(m.to_owned())
        } else {
            Some(format!("refs/heads/{m}"))
        }
    } else {
        let mb = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
        Some(format!("refs/remotes/{remote}/{mb}"))
    }
}

/// Whether `tracking_ref` (a full `refs/remotes/<remote>/<branch>` ref) is the destination of some
/// remote's fetch refspec — i.e. a genuine remote-tracking branch (Git `check_tracking_branch` /
/// `validate_remote_tracking_branch`). A `refs/remotes/...` ref left over after its remote's fetch
/// refspec was narrowed is NOT a valid tracking branch and must not be used for `--track`.
#[must_use]
pub fn remote_tracking_ref_is_mapped(repo: &Repository, tracking_ref: &str) -> bool {
    let Ok(config) = repo.config() else {
        return false;
    };
    for entry in config.entries() {
        // Match keys of the form `remote.<name>.fetch`.
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        if !rest.ends_with(".fetch") {
            continue;
        }
        let Some(spec) = entry.value.as_deref() else {
            continue;
        };
        if refspec_dst_matches(spec, tracking_ref) {
            return true;
        }
    }
    false
}

/// Does the destination half of a fetch refspec (`[+]<src>:<dst>`) match `target`?
/// Supports a single trailing `*` glob, mirroring Git's refspec matching for the common case.
fn refspec_dst_matches(spec: &str, target: &str) -> bool {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let Some((_src, dst)) = spec.split_once(':') else {
        return false;
    };
    let dst = dst.trim();
    if let Some(prefix) = dst.strip_suffix('*') {
        // Glob: dst = `refs/remotes/<remote>/*`; match any ref under that prefix.
        target.starts_with(prefix) && target.len() > prefix.len()
    } else {
        dst == target
    }
}

/// Compare local branch tip to `base_ref` (full ref like `refs/remotes/origin/main`).
pub fn stat_branch_pair(
    repo: &Repository,
    branch_short: &str,
    base_ref: &str,
    mode: AheadBehindMode,
) -> Result<TrackingStat> {
    let branch_ref = branch_head_ref(branch_short);
    let local_oid = match refs::resolve_ref(&repo.git_dir, &branch_ref) {
        Ok(o) => o,
        Err(_) => {
            return Ok(TrackingStat::Diverged {
                display_name: shorten_tracking_ref(base_ref),
                ahead: 0,
                behind: 0,
            });
        }
    };
    let upstream_oid = match refs::resolve_ref(&repo.git_dir, base_ref) {
        Ok(o) => o,
        Err(_) => {
            return Ok(TrackingStat::Gone {
                display_name: shorten_tracking_ref(base_ref),
            });
        }
    };
    if local_oid == upstream_oid {
        return Ok(TrackingStat::UpToDate);
    }
    if mode == AheadBehindMode::Quick {
        return Ok(TrackingStat::Diverged {
            display_name: shorten_tracking_ref(base_ref),
            ahead: 0,
            behind: 0,
        });
    }
    let (ahead, behind) = count_symmetric_ahead_behind(repo, local_oid, upstream_oid)?;
    Ok(TrackingStat::Diverged {
        display_name: shorten_tracking_ref(base_ref),
        ahead,
        behind,
    })
}

/// Read `status.compareBranches` from `.git/config` (`[status]` section or dotted key).
fn parse_status_compare_branches(config_content: &str) -> Option<String> {
    let mut in_status = false;
    for line in config_content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_status = trimmed.eq_ignore_ascii_case("[status]");
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("status.comparebranches") {
            return trimmed.split_once('=').map(|(_, v)| v.trim().to_owned());
        }
        if in_status && lower.starts_with("comparebranches") {
            return trimmed.split_once('=').map(|(_, v)| v.trim().to_owned());
        }
    }
    None
}

fn parse_compare_branch_specs(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn resolve_compare_full_ref(repo: &Repository, branch_short: &str, token: &str) -> Option<String> {
    let t = token.trim();
    if t.eq_ignore_ascii_case("@{upstream}") || t.eq_ignore_ascii_case("@{u}") {
        let spec = if branch_short.is_empty() {
            "@{u}".to_string()
        } else {
            format!("{branch_short}@{{u}}")
        };
        resolve_upstream_symbolic_name(repo, &spec).ok()
    } else if t.eq_ignore_ascii_case("@{push}") {
        resolve_push_full_ref_for_branch(repo, branch_short).ok()
    } else {
        None
    }
}

/// Multi-branch tracking lines for porcelain long status and checkout (Git `format_tracking_info`).
pub fn format_tracking_info(
    repo: &Repository,
    branch_short: &str,
    mode: AheadBehindMode,
    show_divergence_advice: bool,
) -> Result<String> {
    let config_path = repo.git_dir.join("config");
    let config_raw = fs::read_to_string(&config_path).unwrap_or_default();
    let compare_raw =
        parse_status_compare_branches(&config_raw).unwrap_or_else(|| "@{upstream}".to_string());

    let tokens = parse_compare_branch_specs(&compare_raw);
    if tokens.is_empty() {
        return Ok(String::new());
    }

    let upstream_full = resolve_compare_full_ref(repo, branch_short, "@{upstream}");
    let push_full = resolve_compare_full_ref(repo, branch_short, "@{push}");

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = String::new();
    let mut reported = false;

    for tok in tokens {
        let Some(full_ref) = resolve_compare_full_ref(repo, branch_short, &tok) else {
            continue;
        };
        if !seen.insert(full_ref.clone()) {
            continue;
        }

        let is_upstream = upstream_full.as_ref() == Some(&full_ref);
        let mut is_push = push_full.as_ref() == Some(&full_ref);
        if is_upstream && push_full.as_ref().is_none_or(|p| p == &full_ref) {
            is_push = true;
        }

        let stat = stat_branch_pair(repo, branch_short, &full_ref, mode)?;

        match &stat {
            TrackingStat::Gone { display_name } if is_upstream => {
                if reported {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "Your branch is based on '{display_name}', but the upstream is gone.\n"
                ));
                out.push_str("  (use \"git branch --unset-upstream\" to fixup)\n");
                reported = true;
            }
            TrackingStat::Gone { .. } => {}
            TrackingStat::UpToDate => {
                if reported {
                    out.push('\n');
                }
                let d = shorten_tracking_ref(&full_ref);
                out.push_str(&format!("Your branch is up to date with '{d}'.\n"));
                reported = true;
            }
            TrackingStat::Diverged {
                display_name,
                ahead,
                behind,
            } => {
                if reported {
                    out.push('\n');
                }
                if mode == AheadBehindMode::Quick {
                    out.push_str(&format!(
                        "Your branch and '{display_name}' refer to different commits.\n"
                    ));
                    if is_push {
                        out.push_str("  (use \"git status --ahead-behind\" for details)\n");
                    }
                } else if *ahead > 0 && *behind > 0 {
                    out.push_str(&format!(
                        "Your branch and '{display_name}' have diverged,\n\
and have {ahead} and {behind} different commits each, respectively.\n"
                    ));
                    if show_divergence_advice && is_upstream {
                        out.push_str(
                            "  (use \"git pull\" if you want to integrate the remote branch with yours)\n",
                        );
                    }
                } else if *ahead > 0 {
                    out.push_str(&format!(
                        "Your branch is ahead of '{display_name}' by {ahead} commit{}.\n",
                        if *ahead == 1 { "" } else { "s" }
                    ));
                    if is_push {
                        out.push_str("  (use \"git push\" to publish your local commits)\n");
                    }
                } else {
                    out.push_str(&format!(
                        "Your branch is behind '{display_name}' by {behind} commit{}, and can be fast-forwarded.\n",
                        if *behind == 1 { "" } else { "s" }
                    ));
                    if is_upstream {
                        out.push_str("  (use \"git pull\" to update your local branch)\n");
                    }
                }
                reported = true;
            }
        }
    }

    Ok(out)
}
