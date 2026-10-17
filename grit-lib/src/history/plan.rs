//! In-memory history rewrite planning.

use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "unstable-bench-helpers"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commit::signing::extra_headers_without_commit_signatures;
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
#[cfg(any(test, feature = "unstable-bench-helpers"))]
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
#[cfg(any(test, feature = "unstable-bench-helpers"))]
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
/// Returns [`HistoryError`] for conflicts, unsupported merges, scope violations, empty-policy
/// aborts, [`HistoryError::Planning`] for invalid planning inputs, and [`HistoryError::Library`]
/// for object/revwalk/ref failures.
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

        if matches!(edit, Some(CommitEdit::InsertBefore(_))) && old.parents.len() > 1 {
            return Err(HistoryError::InsertBeforeOnMerge { commit: old_oid });
        }

        if matches!(edit, Some(CommitEdit::Drop)) {
            if old.parents.is_empty() {
                return Err(HistoryError::RootCommit { commit: old_oid });
            }
            let parent = mapped_first_parent(&mapping, &old, old_oid)?;
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
                vec![mapped_first_parent(&mapping, &old, old_oid)?]
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
                    vec![mapped_first_parent(&mapping, &old, old_oid)?]
                };
                let author = author.as_deref().unwrap_or(&old.author);
                let message = message.as_deref().unwrap_or(&old.message);
                let oid = if let Some(explicit_tree) = *tree {
                    write_rewritten_commit(
                        repo,
                        &old,
                        explicit_tree,
                        &parent_list,
                        author,
                        &opts.committer,
                        message,
                    )?
                } else {
                    replay_edited_commit(
                        repo,
                        &old,
                        old_oid,
                        &parent_list,
                        author,
                        &opts.committer,
                        message,
                        opts,
                    )?
                };
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
    if let Error::History(inner) = err {
        return inner;
    }
    HistoryError::Library(Box::new(err))
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
        return Err(library_err(Error::CorruptObject(format!(
            "expected commit, got {}",
            raw.kind.as_str()
        ))));
    }
    parse_commit(&raw.data).map_err(library_err)
}

fn mapped_first_parent(
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    commit: ObjectId,
) -> Result<ObjectId, HistoryError> {
    let parent = *old
        .parents
        .first()
        .ok_or(HistoryError::RootCommit { commit })?;
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
    let (_, new_parent) = replay_parent_pair(repo, old, mapping, extra_parent)?;
    replay_edited_commit(
        repo,
        old,
        old_oid,
        &[new_parent],
        &old.author,
        &opts.committer,
        &old.message,
        opts,
    )
}

/// Replay with author/message overrides and an explicit parent list (implicit tree).
#[allow(clippy::too_many_arguments)]
fn replay_edited_commit(
    repo: &Repository,
    old: &CommitData,
    old_oid: ObjectId,
    parent_list: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
    opts: &RewriteOptions,
) -> Result<ObjectId, HistoryError> {
    let empty = empty_tree_oid(repo)?;
    let old_parent = old.parents.first().copied().unwrap_or(empty);
    let new_parent = parent_list.first().copied().unwrap_or(empty);
    let old_parent_tree = if old.parents.is_empty() {
        empty
    } else {
        commit_tree(repo, old_parent)?
    };
    let new_parent_tree = if parent_list.is_empty() {
        empty
    } else {
        commit_tree(repo, new_parent)?
    };

    let result_tree = compute_replayed_tree(repo, old, old_oid, old_parent_tree, new_parent_tree)?;

    if old.tree != old_parent_tree && result_tree == new_parent_tree {
        return match opts.empty {
            EmptyPolicy::Abort => Err(HistoryError::BecameEmpty { commit: old_oid }),
            EmptyPolicy::Drop => Ok(new_parent),
            EmptyPolicy::Keep => write_rewritten_commit(
                repo,
                old,
                result_tree,
                parent_list,
                author,
                committer,
                message,
            ),
        };
    }

    write_rewritten_commit(
        repo,
        old,
        result_tree,
        parent_list,
        author,
        committer,
        message,
    )
}

fn replay_parent_pair(
    repo: &Repository,
    old: &CommitData,
    mapping: &HashMap<ObjectId, ObjectId>,
    extra_parent: Option<ObjectId>,
) -> Result<(ObjectId, ObjectId), HistoryError> {
    let empty = empty_tree_oid(repo)?;
    if old.parents.is_empty() {
        let new_parent = extra_parent.unwrap_or(empty);
        return Ok((empty, new_parent));
    }
    let old_parent = old.parents[0];
    let new_parent =
        extra_parent.unwrap_or_else(|| mapping.get(&old_parent).copied().unwrap_or(old_parent));
    Ok((old_parent, new_parent))
}

fn empty_tree_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    repo.odb().write(ObjectKind::Tree, &[]).map_err(library_err)
}

fn compute_replayed_tree(
    repo: &Repository,
    old: &CommitData,
    old_oid: ObjectId,
    old_parent_tree: ObjectId,
    new_parent_tree: ObjectId,
) -> Result<ObjectId, HistoryError> {
    if old_parent_tree == new_parent_tree {
        return Ok(old.tree);
    }
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
    Ok(merge.result_tree)
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
        extra_headers: extra_headers_without_commit_signatures(&template.extra_headers),
    };
    write_commit_data(repo, &data)
}

fn write_commit_data(repo: &Repository, data: &CommitData) -> Result<ObjectId, HistoryError> {
    let bytes = serialize_commit(data);
    repo.odb()
        .write(ObjectKind::Commit, &bytes)
        .map_err(library_err)
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
    let head_ref = match &head {
        crate::state::HeadState::Branch { refname, .. } => Some(refname.clone()),
        crate::state::HeadState::Detached { .. } | crate::state::HeadState::Invalid => None,
    };
    Ok(HeadEffect {
        commit_changes: new_oid != old_oid,
        old_commit: Some(old_oid),
        head_ref,
        old_tree,
        new_tree,
    })
}

#[cfg(test)]
mod merge_call_tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::index::write_tree::write_tree_from_index;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::refs::write_ref;

    fn ident(n: i64) -> String {
        format!("T <t@grit.test> {n} +0000")
    }

    fn blob_tree(repo: &Repository, path: &str, bytes: &[u8]) -> crate::error::Result<ObjectId> {
        let blob = repo.odb().write(ObjectKind::Blob, bytes)?;
        let mut index = Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: bytes.len() as u32,
            oid: blob,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        });
        write_tree_from_index(repo.odb(), &index, "")
    }

    #[test]
    fn reword_parent_only_replay_skips_merge_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let tree_a = blob_tree(&repo, "f.txt", b"a\n").expect("tree");
        let a = write_new_commit(&repo, tree_a, &[], &id, &id, "a\n", &[]).expect("a");
        let b = write_new_commit(&repo, tree_a, &[a], &id, &id, "b\n", &[]).expect("b");
        write_ref(repo.git_dir(), "refs/heads/main", &b).expect("ref");
        let open =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
        let mut edits = HistoryEdits::new();
        edits.insert(
            a,
            CommitEdit::Rewrite {
                tree: None,
                message: Some("reword\n".into()),
                author: None,
                parents: None,
            },
        );
        test_reset_merge_tree_call_count();
        plan_rewrite(
            &open,
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: ident(2),
            },
        )
        .expect("plan");
        assert_eq!(test_merge_tree_call_count(), 0);
    }
}
