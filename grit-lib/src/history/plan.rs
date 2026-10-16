//! In-memory history rewrite planning.

use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "unstable-bench-helpers"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commit_encoding::identity_raw_for_serialized_commit;
use crate::error::Error;
use crate::merge_base::is_ancestor;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::state::resolve_head;
use crate::worktree::{find_worktree_with_branch_checked_out, worktree_admin_dir};

use super::error::HistoryError;
use super::types::{
    CommitEdit, EmptyPolicy, HeadEffect, HistoryEdits, PlannedRefUpdate, RefScope, RewriteOptions,
    RewritePlan, RewrittenCommit,
};

#[cfg(any(test, feature = "unstable-bench-helpers"))]
static MERGE_TREE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Test hook: number of three-way tree merges performed by planning.
#[doc(hidden)]
#[must_use]
pub fn test_merge_tree_call_count() -> usize {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    {
        MERGE_TREE_CALLS.load(Ordering::Relaxed)
    }
    #[cfg(not(any(test, feature = "unstable-bench-helpers")))]
    {
        0
    }
}

/// Reset [`test_merge_tree_call_count`].
#[doc(hidden)]
pub fn test_reset_merge_tree_call_count() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    MERGE_TREE_CALLS.store(0, Ordering::Relaxed);
}

fn record_merge_call() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    MERGE_TREE_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// Build a rewrite plan: walk affected history, write replacement commits to the ODB, and
/// record ref movements without touching refs, the index, or the worktree.
///
/// # Errors
///
/// Returns [`HistoryError`] for conflicts, unsupported merges, scope violations, and empty-policy
/// aborts. Wraps object and revwalk failures as [`HistoryError::Conflict`] with a message path.
pub fn plan_rewrite(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    if edits.is_empty() {
        return Ok(RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        });
    }

    let oldest = oldest_edited_commit(repo, edits)?;
    let scoped_refs = collect_scoped_refs(repo, opts, oldest)?;
    ensure_branches_not_checked_out_elsewhere(repo, &scoped_refs)?;

    let parents_of_oldest = commit_parents(repo, oldest)?;
    let tips: Vec<String> = scoped_refs
        .iter()
        .map(|(_, oid)| oid.to_hex())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let negative: Vec<String> = parents_of_oldest.iter().map(ObjectId::to_hex).collect();

    let walk_opts = RevListOptions {
        ancestry_path: true,
        ancestry_path_bottoms: vec![oldest],
        ordering: OrderingMode::Topo,
        reverse: true,
        use_commit_graph: true,
        full_history: true,
        ..RevListOptions::default()
    };

    let walk = rev_list(repo, &tips, &negative, &walk_opts).map_err(library_err)?;
    let mut commits_to_replay: Vec<ObjectId> = walk.commits;
    if !commits_to_replay.contains(&oldest) {
        commits_to_replay.insert(0, oldest);
    }

    for &edited in edits.keys() {
        if !commits_to_replay.contains(&edited) {
            return Err(HistoryError::NotInHistory { commit: edited });
        }
    }

    if matches!(opts.scope, RefScope::Head) {
        let head_oid = head_oid(repo)?;
        for &edited in edits.keys() {
            if !is_ancestor(repo, edited, head_oid).map_err(library_err)? {
                return Err(HistoryError::NotInHistory { commit: edited });
            }
        }
    }

    for (name, tip) in &scoped_refs {
        if is_ancestor(repo, oldest, *tip).map_err(library_err)? && !commits_to_replay.contains(tip)
        {
            return Err(HistoryError::DivergentBranch { name: name.clone() });
        }
    }

    let mut mapping: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut rewritten: Vec<RewrittenCommit> = Vec::with_capacity(commits_to_replay.len());

    for old_oid in commits_to_replay {
        let old = load_commit(repo, old_oid)?;
        let edit = edits.get(&old_oid);

        if old.parents.len() > 1 && edit.is_none() {
            return Err(HistoryError::MergeCommitInReplay { commit: old_oid });
        }

        if matches!(edit, Some(CommitEdit::Drop)) {
            if old.parents.is_empty() {
                return Err(HistoryError::RootCommit { commit: old_oid });
            }
            let parent = mapped_first_parent(repo, &mapping, &old)?;
            mapping.insert(old_oid, parent);
            rewritten.push(RewrittenCommit {
                old: old_oid,
                new: None,
            });
            continue;
        }

        let insert_parent = if let Some(CommitEdit::InsertBefore(insert)) = edit {
            let parents = if old.parents.is_empty() {
                Vec::new()
            } else {
                vec![mapped_first_parent(repo, &mapping, &old)?]
            };
            Some(write_new_commit(
                repo,
                insert.tree,
                &parents,
                &insert.author,
                &opts.committer,
                &insert.message,
                &[],
            )?)
        } else {
            None
        };

        let (new_oid, new_field) = match edit {
            None => {
                let oid = replay_descendant(repo, &mapping, &old, old_oid, insert_parent, opts)?;
                (oid, Some(oid))
            }
            Some(CommitEdit::InsertBefore(_)) => {
                let oid = replay_descendant(repo, &mapping, &old, old_oid, insert_parent, opts)?;
                (oid, Some(oid))
            }
            Some(CommitEdit::Rewrite {
                tree,
                message,
                author,
                parents,
            }) => {
                let parent_list = if let Some(parents) = parents {
                    parents.clone()
                } else if let Some(inserted) = insert_parent {
                    vec![inserted]
                } else if old.parents.is_empty() {
                    Vec::new()
                } else if old.parents.len() > 1 {
                    old.parents
                        .iter()
                        .map(|p| mapping.get(p).copied().unwrap_or(*p))
                        .collect()
                } else {
                    vec![mapped_first_parent(repo, &mapping, &old)?]
                };
                let author = author.as_deref().unwrap_or(&old.author);
                let message = message.as_deref().unwrap_or(&old.message);
                let oid = write_rewritten_commit(
                    repo,
                    &old,
                    tree.unwrap_or(old.tree),
                    &parent_list,
                    author,
                    &opts.committer,
                    message,
                )?;
                (oid, Some(oid))
            }
            Some(CommitEdit::Drop) => unreachable!("handled above"),
        };

        mapping.insert(old_oid, new_oid);
        rewritten.push(RewrittenCommit {
            old: old_oid,
            new: new_field,
        });
    }

    let ref_updates = plan_ref_updates(&scoped_refs, &mapping);
    let head = head_effect(repo, &mapping)?;

    Ok(RewritePlan {
        commits: rewritten,
        ref_updates,
        head,
    })
}

fn library_err(err: Error) -> HistoryError {
    HistoryError::Conflict {
        commit: ObjectId::null(crate::objects::HashAlgo::Sha1),
        paths: vec![err.to_string()],
    }
}

fn oldest_edited_commit(repo: &Repository, edits: &HistoryEdits) -> Result<ObjectId, HistoryError> {
    let keys: Vec<ObjectId> = edits.keys().copied().collect();
    for &candidate in &keys {
        let mut is_oldest = true;
        for &other in &keys {
            if other == candidate {
                continue;
            }
            if is_ancestor(repo, other, candidate).map_err(library_err)? {
                is_oldest = false;
                break;
            }
        }
        if is_oldest {
            return Ok(candidate);
        }
    }
    let fallback = edits
        .keys()
        .next()
        .copied()
        .unwrap_or_else(|| ObjectId::null(repo.odb().hash_algo()));
    Err(HistoryError::NotInHistory { commit: fallback })
}

fn head_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
    head.oid().copied().ok_or(HistoryError::NotInHistory {
        commit: ObjectId::null(repo.odb().hash_algo()),
    })
}

fn collect_scoped_refs(
    repo: &Repository,
    opts: &RewriteOptions,
    oldest: ObjectId,
) -> Result<Vec<(String, ObjectId)>, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
    let mut out = Vec::new();

    match opts.scope {
        RefScope::Head => match head {
            crate::state::HeadState::Branch { refname, oid, .. } => {
                let Some(oid) = oid else {
                    return Err(HistoryError::NotInHistory { commit: oldest });
                };
                if !is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    return Err(HistoryError::NotInHistory { commit: oldest });
                }
                out.push((refname, oid));
            }
            crate::state::HeadState::Detached { oid } => {
                if !is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    return Err(HistoryError::NotInHistory { commit: oldest });
                }
                out.push(("HEAD".to_owned(), oid));
            }
            crate::state::HeadState::Invalid => {
                return Err(HistoryError::NotInHistory { commit: oldest });
            }
        },
        RefScope::Branches => {
            let branches = repo.references().list("refs/heads/").map_err(library_err)?;
            for (name, oid) in branches {
                if is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    out.push((name, oid));
                }
            }
            if let crate::state::HeadState::Detached { oid } = head {
                if is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    out.push(("HEAD".to_owned(), oid));
                }
            }
        }
    }

    if out.is_empty() {
        return Err(HistoryError::NotInHistory { commit: oldest });
    }
    Ok(out)
}

fn ensure_branches_not_checked_out_elsewhere(
    repo: &Repository,
    scoped_refs: &[(String, ObjectId)],
) -> Result<(), HistoryError> {
    let here = worktree_admin_dir(repo);
    for (name, _) in scoped_refs {
        let short = name.strip_prefix("refs/heads/").unwrap_or(name.as_str());
        if short == "HEAD" {
            continue;
        }
        if let Some(found) =
            find_worktree_with_branch_checked_out(repo, short, Some(&here)).map_err(library_err)?
        {
            return Err(HistoryError::BranchCheckedOutElsewhere {
                name: name.clone(),
                worktree: found.worktree_path.display().to_string(),
            });
        }
    }
    Ok(())
}

fn commit_parents(repo: &Repository, oid: ObjectId) -> Result<Vec<ObjectId>, HistoryError> {
    Ok(load_commit(repo, oid)?.parents)
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    let raw = repo.odb().read(&oid).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(HistoryError::Conflict {
            commit: oid,
            paths: vec!["object is not a commit".to_owned()],
        });
    }
    parse_commit(&raw.data).map_err(library_err)
}

fn mapped_first_parent(
    repo: &Repository,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
) -> Result<ObjectId, HistoryError> {
    let parent = *old.parents.first().ok_or(HistoryError::RootCommit {
        commit: ObjectId::null(repo.odb().hash_algo()),
    })?;
    Ok(mapping.get(&parent).copied().unwrap_or(parent))
}

fn replay_descendant(
    repo: &Repository,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    old_oid: ObjectId,
    extra_parent: Option<ObjectId>,
    opts: &RewriteOptions,
) -> Result<ObjectId, HistoryError> {
    let old_parent = *old
        .parents
        .first()
        .ok_or(HistoryError::RootCommit { commit: old_oid })?;
    let new_parent =
        extra_parent.unwrap_or_else(|| mapping.get(&old_parent).copied().unwrap_or(old_parent));

    let old_parent_tree = commit_tree(repo, old_parent)?;
    let new_parent_tree = commit_tree(repo, new_parent)?;

    let result_tree = if old_parent_tree == new_parent_tree {
        old.tree
    } else {
        record_merge_call();
        let merge = merge_trees_three_way(
            repo,
            old_parent_tree,
            new_parent_tree,
            old.tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
        )
        .map_err(library_err)?;

        if merge.index.has_unmerged_entries() || !merge.conflict_content.is_empty() {
            return Err(HistoryError::Conflict {
                commit: old_oid,
                paths: conflict_paths(&merge),
            });
        }
        merge.result_tree
    };

    let old_tree = old.tree;
    if old_tree != old_parent_tree && result_tree == new_parent_tree {
        return match opts.empty {
            EmptyPolicy::Abort => Err(HistoryError::BecameEmpty { commit: old_oid }),
            EmptyPolicy::Drop => Ok(new_parent),
            EmptyPolicy::Keep => write_rewritten_commit(
                repo,
                old,
                result_tree,
                &[new_parent],
                &old.author,
                &opts.committer,
                &old.message,
            ),
        };
    }

    write_rewritten_commit(
        repo,
        old,
        result_tree,
        &[new_parent],
        &old.author,
        &opts.committer,
        &old.message,
    )
}

fn conflict_paths(merge: &crate::merge_trees::TreeMergeOutput) -> Vec<String> {
    let mut paths = HashSet::new();
    for key in merge.conflict_content.keys() {
        paths.insert(String::from_utf8_lossy(key).into_owned());
    }
    for entry in &merge.index.entries {
        if entry.stage() != 0 {
            paths.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    paths.into_iter().collect()
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, HistoryError> {
    Ok(load_commit(repo, commit)?.tree)
}

fn write_new_commit(
    repo: &Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
    extra_headers: &[u8],
) -> Result<ObjectId, HistoryError> {
    let data = CommitData {
        tree,
        parents: parents.to_vec(),
        author: author.to_owned(),
        committer: committer.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: message.to_owned(),
        raw_message: None,
        extra_headers: extra_headers.to_vec(),
    };
    write_commit_data(repo, &data)
}

/// Write a replacement commit, preserving encoding and raw author/message bytes from `template`
/// when those fields are unchanged while always applying the new committer.
fn write_rewritten_commit(
    repo: &Repository,
    template: &CommitData,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
) -> Result<ObjectId, HistoryError> {
    let author_unchanged = author == template.author;
    let message_unchanged = message == template.message;
    let encoding = if message_unchanged {
        template.encoding.clone()
    } else {
        None
    };
    let raw_message = if message_unchanged {
        template.raw_message.clone()
    } else {
        None
    };
    let (author_raw, committer_raw) = if author_unchanged && !template.author_raw.is_empty() {
        let (_, committer_raw) = identity_raw_for_serialized_commit(&encoding, author, committer);
        (template.author_raw.clone(), committer_raw)
    } else {
        identity_raw_for_serialized_commit(&encoding, author, committer)
    };
    let data = CommitData {
        tree,
        parents: parents.to_vec(),
        author: author.to_owned(),
        committer: committer.to_owned(),
        author_raw,
        committer_raw,
        encoding,
        message: message.to_owned(),
        raw_message,
        extra_headers: strip_gpgsig(&template.extra_headers),
    };
    write_commit_data(repo, &data)
}

fn write_commit_data(repo: &Repository, data: &CommitData) -> Result<ObjectId, HistoryError> {
    let bytes = serialize_commit(data);
    repo.odb()
        .write(ObjectKind::Commit, &bytes)
        .map_err(library_err)
}

/// Drop OpenPGP signature headers; they are invalid after any rewrite.
fn strip_gpgsig(extra_headers: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < extra_headers.len() {
        let line_start = i;
        while i < extra_headers.len() && extra_headers[i] != b'\n' {
            i += 1;
        }
        let line_end = i;
        if i < extra_headers.len() {
            i += 1;
        }
        let line = &extra_headers[line_start..line_end];
        if line.starts_with(b"gpgsig ") {
            while i < extra_headers.len() {
                if extra_headers[i] == b'\n' {
                    i += 1;
                    if i >= extra_headers.len() || extra_headers[i] != b' ' {
                        break;
                    }
                } else {
                    while i < extra_headers.len() && extra_headers[i] != b'\n' {
                        i += 1;
                    }
                    if i < extra_headers.len() {
                        i += 1;
                    }
                }
            }
            continue;
        }
        out.extend_from_slice(&extra_headers[line_start..line_end]);
        out.push(b'\n');
    }
    out
}

fn plan_ref_updates(
    scoped_refs: &[(String, ObjectId)],
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Vec<PlannedRefUpdate> {
    let mut updates = Vec::new();
    for (name, old_tip) in scoped_refs {
        let new_tip = resolve_mapped_tip(*old_tip, mapping);
        if new_tip != *old_tip {
            updates.push(PlannedRefUpdate {
                name: name.clone(),
                old: *old_tip,
                new: new_tip,
            });
        }
    }
    updates.sort_by(|a, b| a.name.cmp(&b.name));
    updates
}

fn resolve_mapped_tip(tip: ObjectId, mapping: &HashMap<ObjectId, ObjectId>) -> ObjectId {
    mapping.get(&tip).copied().unwrap_or(tip)
}

fn head_effect(
    repo: &Repository,
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Result<HeadEffect, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
    let old_oid = match head {
        crate::state::HeadState::Branch { oid, .. } => oid,
        crate::state::HeadState::Detached { oid } => Some(oid),
        crate::state::HeadState::Invalid => None,
    };
    let Some(old_oid) = old_oid else {
        return Ok(HeadEffect::default());
    };
    let new_oid = mapping.get(&old_oid).copied().unwrap_or(old_oid);
    let old_tree = load_commit(repo, old_oid).ok().map(|c| c.tree);
    let new_tree = load_commit(repo, new_oid).ok().map(|c| c.tree);
    Ok(HeadEffect {
        commit_changes: new_oid != old_oid,
        old_tree,
        new_tree,
    })
}
