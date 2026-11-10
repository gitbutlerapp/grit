//! Case-insensitive path groups that cannot be staged or committed on this work tree.

use std::collections::HashSet;

use crate::error::Result;
use crate::index::{Index, MODE_EXECUTABLE, MODE_REGULAR};
use crate::path_icase::{
    worktree_path_collision_options, worktree_path_for_index_entry_with_options,
    IcaseCheckoutCollisionMap,
};
use crate::repo::{Repository, RepositoryError};

/// Tracked paths that collide on a case-insensitive volume and share one on-disk file.
///
/// Returned when the work tree cannot faithfully represent every member of the group
/// (a collision loser is absent or the dentry holds a sibling's blob).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaseCollisionGroup {
    /// Index spellings in the collision group, sorted.
    pub paths: Vec<String>,
}

/// Whether `path` is listed in any [`CaseCollisionGroup`].
#[must_use]
pub fn path_in_worktree_case_collision(path: &str, groups: &[CaseCollisionGroup]) -> bool {
    groups.iter().any(|g| g.paths.iter().any(|p| p == path))
}

/// All paths belonging to active work-tree case-collision groups.
#[must_use]
pub fn worktree_case_collision_paths(groups: &[CaseCollisionGroup]) -> HashSet<String> {
    groups
        .iter()
        .flat_map(|g| g.paths.iter().cloned())
        .collect()
}

/// Discover collision groups that block staging and commit on this work tree.
///
/// Active when `core.ignorecase` is enabled and, for at least one collision loser in the group,
/// the resolved work-tree path is missing or the dentry bytes hash to a sibling index blob.
///
/// # Errors
///
/// Propagates repository, config, and work-tree hash failures.
pub fn worktree_case_collision_groups(
    repo: &Repository,
    index: &Index,
) -> Result<Vec<CaseCollisionGroup>> {
    let config = repo.config().snapshot()?;
    let collision_opts =
        worktree_path_collision_options(config.ignore_case(), config.precompose_unicode());
    if !collision_opts.active() {
        return Ok(Vec::new());
    }
    let work_tree = repo.work_tree().ok_or(RepositoryError::RequiresWorkTree)?;
    let map = IcaseCheckoutCollisionMap::from_index(work_tree, index, collision_opts);
    let groups = map.collision_groups();
    if groups.member_groups().is_empty() {
        return Ok(Vec::new());
    }
    let conv = crate::crlf::ConversionConfig::from_config(config.as_ref());
    let attrs = crate::crlf::load_gitattributes(work_tree);
    let mut out = Vec::new();
    for members in groups.member_groups() {
        let mut active = false;
        for member in members {
            let path_bytes = member.as_bytes();
            if !map.is_auto_stage_loser(path_bytes) {
                continue;
            }
            let abs = worktree_path_for_index_entry_with_options(work_tree, member, collision_opts);
            let Ok(meta) = std::fs::symlink_metadata(&abs) else {
                active = true;
                break;
            };
            if meta.is_dir() {
                continue;
            }
            let Some(ie) = index.get(path_bytes, 0) else {
                continue;
            };
            if ie.mode != MODE_REGULAR && ie.mode != MODE_EXECUTABLE {
                continue;
            }
            let file_attrs = crate::crlf::get_file_attrs(&attrs, member, false, config.as_ref());
            let worktree_oid = match crate::diff::hash_worktree_file(
                repo.odb(),
                &abs,
                &meta,
                &conv,
                &file_attrs,
                member,
                Some(ie),
                None,
            ) {
                Ok(oid) => oid,
                Err(_) => {
                    active = true;
                    break;
                }
            };
            if map.worktree_oid_is_collision_sibling_blob(path_bytes, worktree_oid, ie.oid) {
                active = true;
                break;
            }
        }
        if active {
            let mut paths: Vec<String> = members.clone();
            paths.sort();
            out.push(CaseCollisionGroup { paths });
        }
    }
    out.sort_by(|a, b| a.paths.cmp(&b.paths));
    Ok(out)
}

/// Adjust [`crate::porcelain::status::StatusGuidance`] when unstaged paths are only case-collision drift.
#[must_use]
pub fn filter_guidance_for_case_collisions(
    guidance: crate::porcelain::status::StatusGuidance,
    unstaged_paths: &[String],
    blocked_paths: &HashSet<String>,
) -> crate::porcelain::status::StatusGuidance {
    if blocked_paths.is_empty() {
        return guidance;
    }
    let only_collision_unstaged =
        !unstaged_paths.is_empty() && unstaged_paths.iter().all(|p| blocked_paths.contains(p));
    if !only_collision_unstaged {
        return guidance;
    }
    match guidance {
        crate::porcelain::status::StatusGuidance::Actions(actions) => {
            let filtered: Vec<_> = actions
                .into_iter()
                .filter(|action| {
                    !matches!(
                        action,
                        crate::porcelain::status::StatusAction::Stage
                            | crate::porcelain::status::StatusAction::DiscardWorktreeChanges
                    )
                })
                .collect();
            crate::porcelain::status::StatusGuidance::Actions(filtered)
        }
        other => other,
    }
}
