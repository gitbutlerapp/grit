//! Switch branches or detach `HEAD` at a commit, updating the working tree when needed.

use std::collections::{HashMap, HashSet};

use crate::check_ref_format::validate_branch_short_name;
use crate::diff::{DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::checkout::checkout_tree_changes;
use crate::porcelain::checkout_reflog::{
    append_head_checkout_reflog, checkout_reflog_from_label, finish_switch_ref_updates,
    is_already_detached_at, is_already_on_branch, switch_create_reflog_source_label,
};
use crate::porcelain::worktree_guard::{
    ensure_no_untracked_overwrite, load_worktree_snapshot, local_change_diffs, TreeSwitchPlan,
};
use crate::refs;
use crate::repo::Repository;
use crate::rev_parse::expand_at_minus_to_branch_name;
use crate::rev_parse::resolve_revision;
use crate::state::resolve_head;

/// Resolved switch target after DWIM and revision parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchMode {
    /// Check out an existing local branch.
    Existing {
        /// Short branch name (`main`, `feature`, …).
        branch: String,
        /// Tip commit of that branch.
        oid: ObjectId,
    },
    /// Create `refs/heads/<branch>` at `start_oid`, then check it out.
    Create {
        branch: String,
        start_oid: ObjectId,
        /// Label for `branch: Created from …` (e.g. `HEAD`, `main`, `refs/remotes/origin/topic`).
        create_source_label: String,
    },
    /// Detached `HEAD` at `commit`.
    Detached {
        commit: ObjectId,
        /// User-facing revision string for the reflog `to` label (e.g. `HEAD~1`).
        reflog_to_label: String,
    },
}

/// Inputs for [`switch_repository`].
#[derive(Debug, Clone)]
pub struct SwitchOptions {
    pub mode: SwitchMode,
}

/// Result of [`switch_repository`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// Short branch name when attached; empty when detached.
    pub branch: String,
    pub created: bool,
    pub detached: bool,
    /// Commit `HEAD` resolves to after the switch.
    pub commit: ObjectId,
}

struct PreparedSwitch {
    plan: TreeSwitchPlan,
    /// Tree-diff paths whose unstaged worktree edits must not be overwritten.
    preserve_worktree_paths: HashSet<String>,
}

/// Resolve `-c <branch> [<start-point>]`.
pub fn resolve_create_branch(
    repo: &Repository,
    branch: &str,
    start_point: Option<&str>,
) -> Result<SwitchMode> {
    validate_branch_short_name(branch)
        .map_err(|e| Error::Message(format!("invalid branch name '{branch}': {e}")))?;
    let branch_ref = format!("refs/heads/{branch}");
    if refs::resolve_ref(&repo.git_dir, &branch_ref).is_ok() {
        return Err(Error::Message(format!("branch '{branch}' already exists")));
    }
    let start_oid = match start_point {
        Some(spec) => resolve_revision_spec(repo, spec)?,
        None => resolve_head(&repo.git_dir)?
            .oid()
            .copied()
            .ok_or_else(|| Error::Message("no commits yet to create a branch from".into()))?,
    };
    Ok(SwitchMode::Create {
        branch: branch.to_owned(),
        start_oid,
        create_source_label: switch_create_reflog_source_label(start_point),
    })
}

/// Resolve a branch name, remote-tracking branch, or revision for `grit switch`.
pub fn resolve_switch_target(repo: &Repository, spec: &str) -> Result<SwitchMode> {
    let spec = normalize_switch_spec(spec);
    if let Ok(Some(branch)) = expand_at_minus_to_branch_name(repo, spec) {
        let oid = refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{branch}"))?;
        return Ok(SwitchMode::Existing { branch, oid });
    }

    if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{spec}")) {
        return Ok(SwitchMode::Existing {
            branch: spec.to_owned(),
            oid,
        });
    }

    for candidate in [
        format!("refs/remotes/{spec}"),
        format!("refs/remotes/origin/{spec}"),
    ] {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &candidate) {
            let short = spec.strip_prefix("origin/").unwrap_or(spec).to_owned();
            validate_branch_short_name(&short)
                .map_err(|e| Error::Message(format!("invalid branch name '{short}': {e}")))?;
            let local = format!("refs/heads/{short}");
            if let Ok(local_oid) = refs::resolve_ref(&repo.git_dir, &local) {
                return Ok(SwitchMode::Existing {
                    branch: short,
                    oid: local_oid,
                });
            }
            return Ok(SwitchMode::Create {
                branch: short,
                start_oid: oid,
                create_source_label: candidate,
            });
        }
    }

    if let Some(mode) = resolve_unique_dwim_switch(repo, spec)? {
        return Ok(mode);
    }

    Err(Error::Message(format!("no branch named '{spec}'")))
}

fn normalize_switch_spec(spec: &str) -> &str {
    if spec == "-" {
        "@{-1}"
    } else {
        spec
    }
}

fn resolve_unique_dwim_switch(repo: &Repository, spec: &str) -> Result<Option<SwitchMode>> {
    const DWIM_RULES: &[&str] = &[
        "{0}",
        "refs/{0}",
        "refs/tags/{0}",
        "refs/heads/{0}",
        "refs/remotes/{0}",
        "refs/remotes/{0}/HEAD",
    ];
    let mut hits: Vec<(String, ObjectId)> = Vec::new();
    for rule in DWIM_RULES {
        let candidate = rule.replace("{0}", spec);
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &candidate) {
            hits.push((candidate, oid));
        }
    }
    if hits.len() != 1 {
        return Ok(None);
    }
    let Some((refname, oid)) = hits.into_iter().next() else {
        return Ok(None);
    };
    if let Some(short) = refname.strip_prefix("refs/heads/") {
        return Ok(Some(SwitchMode::Existing {
            branch: short.to_owned(),
            oid,
        }));
    }
    if refname.starts_with("refs/tags/") {
        return Err(Error::Message(format!(
            "cannot switch to tag '{spec}' on a branch — use `grit switch --detach {spec}`"
        )));
    }
    Ok(None)
}

/// Resolve a revision expression for `--detach` or `-c` start points.
pub fn resolve_revision_spec(repo: &Repository, spec: &str) -> Result<ObjectId> {
    resolve_revision(repo, spec).map_err(|_| Error::Message(format!("invalid revision '{spec}'")))
}

/// Perform a branch switch or detached checkout.
pub fn switch_repository(repo: &Repository, opts: &SwitchOptions) -> Result<SwitchOutcome> {
    let (target_oid, branch_ref, created, detached, branch_label, pending_create, reflog_to) =
        match &opts.mode {
            SwitchMode::Existing { branch, oid } => (
                *oid,
                format!("refs/heads/{branch}"),
                false,
                false,
                branch.clone(),
                None,
                branch.clone(),
            ),
            SwitchMode::Create {
                branch,
                start_oid,
                create_source_label,
            } => (
                *start_oid,
                format!("refs/heads/{branch}"),
                true,
                false,
                branch.clone(),
                Some((
                    format!("refs/heads/{branch}"),
                    *start_oid,
                    create_source_label.clone(),
                )),
                branch.clone(),
            ),
            SwitchMode::Detached {
                commit,
                reflog_to_label,
            } => (
                *commit,
                String::new(),
                false,
                true,
                String::new(),
                None,
                reflog_to_label.clone(),
            ),
        };

    if let SwitchMode::Existing { branch, .. } = &opts.mode {
        crate::worktree::ensure_branch_not_checked_out_elsewhere(repo, branch)?;
    }

    if !detached && is_already_on_branch(repo, &branch_ref, target_oid)? {
        let head_before = resolve_head(&repo.git_dir)?;
        if let Some(from_label) = checkout_reflog_from_label(&head_before) {
            let old_oid = head_before
                .oid()
                .copied()
                .unwrap_or_else(crate::diff::zero_oid);
            append_head_checkout_reflog(repo, old_oid, target_oid, &from_label, &branch_label)?;
        }
        return Ok(SwitchOutcome {
            branch: branch_label,
            created,
            detached,
            commit: target_oid,
        });
    }
    if detached && is_already_detached_at(repo, target_oid)? {
        return Ok(SwitchOutcome {
            branch: branch_label,
            created,
            detached,
            commit: target_oid,
        });
    }

    switch_to_target(
        repo,
        &branch_ref,
        target_oid,
        created,
        detached,
        &branch_label,
        pending_create,
        &reflog_to,
    )
}

fn switch_to_target(
    repo: &Repository,
    branch_ref: &str,
    target_oid: ObjectId,
    created: bool,
    detached: bool,
    branch_label: &str,
    pending_create: Option<(String, ObjectId, String)>,
    reflog_to_label: &str,
) -> Result<SwitchOutcome> {
    let head_before = resolve_head(&repo.git_dir)?;
    let head_oid = head_before.oid().copied();
    let head_tree = head_oid.map(|oid| commit_tree(repo, &oid)).transpose()?;
    let target_tree = commit_tree(repo, &target_oid)?;

    let prepared = prepare_tree_switch_allow_local(repo, head_tree.as_ref(), &target_tree)?;
    ensure_no_untracked_overwrite(&prepared.plan.untracked, &prepared.plan.tree_changes)?;

    let tree_changes: Vec<DiffEntry> = prepared
        .plan
        .tree_changes
        .into_iter()
        .filter(|change| {
            path_from_diff(change)
                .is_none_or(|path| !prepared.preserve_worktree_paths.contains(path))
        })
        .collect();
    checkout_tree_changes(repo, &tree_changes)?;

    if let Some(from_label) = checkout_reflog_from_label(&head_before) {
        finish_switch_ref_updates(
            repo,
            &head_before,
            pending_create,
            detached,
            branch_ref,
            target_oid,
            &from_label,
            reflog_to_label,
        )?;
    } else if let Some((refname, oid, source)) = pending_create {
        crate::porcelain::checkout_reflog::create_branch_ref_with_reflog(
            repo, &refname, oid, &source,
        )?;
        if detached {
            refs::write_ref(&repo.git_dir, "HEAD", &target_oid)?;
        } else {
            refs::write_symbolic_ref(&repo.git_dir, "HEAD", branch_ref)?;
        }
    }

    Ok(SwitchOutcome {
        branch: branch_label.to_owned(),
        created,
        detached,
        commit: target_oid,
    })
}

fn prepare_tree_switch_allow_local(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<PreparedSwitch> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let index_path = repo.index_path();
    let (staged, unstaged, index_changed) =
        local_change_diffs(repo, &mut snapshot.index, snapshot.head_tree)?;
    if index_changed && repo.try_write_index(&mut snapshot.index)? {
        snapshot.index.source_mtime = crate::index::index_file_mtime(&index_path);
    }

    let tree_changes = crate::diff::diff_trees(&repo.odb, from_tree, Some(to_tree), "")?;
    let preserve_worktree_paths = ensure_local_changes_compatible_with_switch(
        &snapshot.index,
        &staged,
        &unstaged,
        &tree_changes,
    )?;

    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let (untracked, _) = crate::porcelain::status::collect_untracked_and_ignored(
        repo,
        &mut snapshot.index,
        work_tree,
        crate::porcelain::status::IgnoredMode::No,
        true,
        &[],
    )?;
    Ok(PreparedSwitch {
        plan: TreeSwitchPlan {
            untracked,
            tree_changes,
        },
        preserve_worktree_paths,
    })
}

fn commit_tree(repo: &Repository, commit: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(commit)?;
    Ok(parse_commit(&obj.data)?.tree)
}

fn path_from_diff(entry: &DiffEntry) -> Option<&str> {
    entry.new_path.as_deref().or(entry.old_path.as_deref())
}

fn ensure_local_changes_compatible_with_switch(
    index: &crate::index::Index,
    staged: &[DiffEntry],
    unstaged: &[DiffEntry],
    tree_changes: &[DiffEntry],
) -> Result<HashSet<String>> {
    let mut target_by_path: HashMap<&str, ObjectId> = HashMap::new();
    for change in tree_changes {
        if let (Some(path), oid) = (path_from_diff(change), change.new_oid) {
            if change.status != DiffStatus::Deleted {
                target_by_path.insert(path, oid);
            }
        }
    }
    let tree_changed_paths: HashSet<&str> =
        tree_changes.iter().filter_map(path_from_diff).collect();

    let mut preserve_worktree_paths = HashSet::new();

    for entry in staged {
        let Some(path) = path_from_diff(entry) else {
            continue;
        };
        if !tree_changed_paths.contains(path) {
            continue;
        }
        if target_by_path.get(path).is_some_and(|target_oid| {
            index
                .get(path.as_bytes(), 0)
                .is_some_and(|e| e.oid == *target_oid)
        }) {
            continue;
        }
        return Err(Error::Message(
            "you have uncommitted changes — commit them before switching".into(),
        ));
    }

    for entry in unstaged {
        let Some(path) = path_from_diff(entry) else {
            continue;
        };
        if !tree_changed_paths.contains(path) {
            continue;
        }
        if target_by_path.get(path).is_some_and(|target_oid| {
            index
                .get(path.as_bytes(), 0)
                .is_some_and(|e| e.oid == *target_oid)
        }) {
            preserve_worktree_paths.insert(path.to_owned());
            continue;
        }
        return Err(Error::Message(
            "you have uncommitted changes — commit them before switching".into(),
        ));
    }

    Ok(preserve_worktree_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{serialize_commit, CommitData, ObjectKind};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::NullProgress;
    use crate::repo::{init_repository, Repository};
    use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
    use crate::RefStorageFormat;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        (dir, repo)
    }

    fn commit_on_branch(
        repo: &Repository,
        branch: &str,
        path: &str,
        contents: &str,
        parent: Option<ObjectId>,
    ) -> ObjectId {
        let wt = repo.work_tree.as_ref().expect("wt");
        std::fs::write(wt.join(path), contents).expect("write");
        let mut index = repo.load_index().expect("index");
        let oid = repo
            .odb
            .write(ObjectKind::Blob, contents.as_bytes())
            .expect("blob");
        let entry = crate::index::entry_from_stat(
            &wt.join(path),
            path.as_bytes(),
            oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        repo.write_index(&mut index).expect("write index");
        let commit = CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "c\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        let refname = format!("refs/heads/{branch}");
        refs::write_ref(&repo.git_dir, &refname, &co).expect("branch");
        refs::write_symbolic_ref(&repo.git_dir, "HEAD", &refname).expect("symhead");
        co
    }

    #[test]
    fn switch_preserves_unstaged_over_staged_destination() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        commit_on_branch(&repo, "other", "f", "two\n", Some(main_tip));
        refs::write_symbolic_ref(&repo.git_dir, "HEAD", "refs/heads/main").expect("head");

        let wt = repo.work_tree.as_ref().expect("wt");
        std::fs::write(wt.join("f"), "two\n").expect("stage dest in index");
        let mut index = repo.load_index().expect("index");
        let oid = repo.odb.write(ObjectKind::Blob, b"two\n").expect("blob");
        index.add_or_replace(
            crate::index::entry_from_stat(&wt.join("f"), b"f", oid, crate::index::MODE_REGULAR)
                .expect("entry"),
        );
        repo.write_index(&mut index).expect("write index");
        std::fs::write(wt.join("f"), "precious\n").expect("unstaged");

        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "other".into(),
                    oid: refs::resolve_ref(&repo.git_dir, "refs/heads/other").expect("oid"),
                },
            },
        )
        .expect("switch");

        assert_eq!(std::fs::read(wt.join("f")).expect("disk"), b"precious\n");
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert_eq!(st.unstaged.len(), 1);
    }

    #[test]
    fn failed_create_switch_leaves_no_branch_ref() {
        let (_dir, repo) = init_repo();
        let main_tip = commit_on_branch(&repo, "main", "f", "one\n", None);
        commit_on_branch(&repo, "other", "f", "two\n", Some(main_tip));
        refs::write_symbolic_ref(&repo.git_dir, "HEAD", "refs/heads/main").expect("head");

        let wt = repo.work_tree.as_ref().expect("wt");
        std::fs::write(wt.join("f"), "three\n").expect("wt");
        let mut index = repo.load_index().expect("index");
        let oid = repo.odb.write(ObjectKind::Blob, b"three\n").expect("blob");
        index.add_or_replace(
            crate::index::entry_from_stat(&wt.join("f"), b"f", oid, crate::index::MODE_REGULAR)
                .expect("entry"),
        );
        repo.write_index(&mut index).expect("write index");

        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Create {
                    branch: "leaked".into(),
                    start_oid: refs::resolve_ref(&repo.git_dir, "refs/heads/other").expect("oid"),
                    create_source_label: "HEAD".into(),
                },
            },
        )
        .expect_err("conflict");
        assert!(matches!(err, Error::Message(_)));
        assert!(refs::resolve_ref(&repo.git_dir, "refs/heads/leaked").is_err());
    }

    #[test]
    fn remote_spec_uses_local_branch_oid_when_branch_exists() {
        let (_dir, repo) = init_repo();
        let local_tip = commit_on_branch(&repo, "main", "f", "local\n", None);
        let remote_tip = commit_on_branch(&repo, "other", "f", "remote\n", Some(local_tip));
        refs::write_ref(&repo.git_dir, "refs/remotes/origin/main", &remote_tip).expect("remote");

        let mode = resolve_switch_target(&repo, "origin/main").expect("resolve");
        match mode {
            SwitchMode::Existing { branch, oid } => {
                assert_eq!(branch, "main");
                assert_eq!(oid, local_tip);
            }
            other => panic!("expected existing local main, got {other:?}"),
        }
    }

    #[test]
    fn dwim_tag_is_rejected_for_attached_switch() {
        let (_dir, repo) = init_repo();
        let tip = commit_on_branch(&repo, "main", "f", "x\n", None);
        refs::write_ref(&repo.git_dir, "refs/tags/rel1", &tip).expect("tag");

        let err = resolve_switch_target(&repo, "rel1").expect_err("tag");
        assert!(matches!(err, Error::Message(_)));
        assert!(err.to_string().contains("--detach"));
    }
}
