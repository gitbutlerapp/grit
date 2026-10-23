//! Move `HEAD` and optionally the index and working tree to a target commit.

use std::collections::HashSet;

use crate::change_id::resolved_change_id;
use crate::config::ConfigSet;
use crate::diff::DiffEntry;
use crate::error::{Error, Result};
use crate::merge_base::{
    is_ancestor, walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::objects::{parse_commit, read_commit_subject_line, ObjectId, ObjectKind};
use crate::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use crate::porcelain::index_from_tree::index_from_tree_with_worktree_stats;
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local_reset, prepare_tree_transition_allow_local,
    TreeTransitionPrepareError,
};
use crate::porcelain::worktree_guard::{load_worktree_snapshot, local_change_diffs};
use crate::refs::{self, append_reflog};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::worktree::list_worktrees;

/// Default cap on [`ResetOptions::orphan_limit`] and orphaned commit listings.
pub const DEFAULT_ORPHAN_LIMIT: usize = 20;

/// Which parts of the repository [`reset_to_commit`] updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResetMode {
    /// Move the branch tip (or detached `HEAD`) only; leave index and worktree unchanged.
    Soft,
    /// Move the tip and reset the index to the target tree; leave the worktree unchanged.
    #[default]
    Mixed,
    /// Move the tip, reset the index, and check out the target tree in the worktree.
    Hard,
    /// Move the tip and merge index/worktree toward the target while keeping unrelated local edits.
    Keep,
}

impl ResetMode {
    /// Lowercase mode name for logging and `--json` output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Mixed => "mixed",
            Self::Hard => "hard",
            Self::Keep => "keep",
        }
    }
}

/// One commit that would become unreachable after resetting away from `previous`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedCommitSummary {
    /// Commit object id.
    pub oid: ObjectId,
    /// Stored or derived change id for the commit.
    pub change_id: String,
    /// First line of the commit message.
    pub subject: String,
}

/// Inputs for [`reset_to_commit`] and [`preview_reset`].
#[derive(Debug, Clone)]
pub struct ResetOptions {
    /// Commit to reset to.
    pub target: ObjectId,
    /// Soft, mixed (default), hard, or keep reset.
    pub mode: ResetMode,
    /// Reflog identity: `Name <email> <unix-time> <tz>`.
    pub identity: String,
    /// Reflog message (e.g. `reset: moving to HEAD~1`).
    pub message: String,
    /// When true, compute effects without updating refs, index, or worktree.
    pub dry_run: bool,
    /// Maximum orphaned commits to return; defaults to [`DEFAULT_ORPHAN_LIMIT`].
    pub orphan_limit: usize,
}

impl ResetOptions {
    /// Build reset options with default dry-run and orphan-list settings.
    #[must_use]
    pub fn new(
        target: ObjectId,
        mode: ResetMode,
        identity: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            target,
            mode,
            identity: identity.into(),
            message: message.into(),
            dry_run: false,
            orphan_limit: DEFAULT_ORPHAN_LIMIT,
        }
    }

    /// Effective orphan listing cap (never zero).
    #[must_use]
    pub fn effective_orphan_limit(&self) -> usize {
        if self.orphan_limit == 0 {
            DEFAULT_ORPHAN_LIMIT
        } else {
            self.orphan_limit
        }
    }
}

/// Non-mutating preview of [`reset_to_commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetPreview {
    /// Tracked paths whose staged or unstaged edits a hard reset would discard.
    pub would_overwrite: Vec<String>,
    /// Paths that would block a keep reset because of local edits on tree-diff paths.
    pub would_block: Vec<String>,
    /// Newest-first sample of commits that would become orphaned.
    pub orphaned_commits: Vec<OrphanedCommitSummary>,
    /// Total orphaned commit count (may exceed `orphaned_commits.len()`).
    pub orphaned_total: usize,
    /// True when `orphaned_total` exceeds the orphan limit.
    pub orphaned_truncated: bool,
}

/// Result of [`reset_to_commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetOutcome {
    /// `soft`, `mixed`, `hard`, or `keep`.
    pub mode: ResetMode,
    /// Commit `HEAD` points at after the reset (unchanged when `dry_run` is set).
    pub target: ObjectId,
    /// Previous `HEAD` commit before the reset.
    pub previous: ObjectId,
    /// Branch ref updated, or `HEAD` when detached.
    pub ref_updated: String,
    /// Whether the operation was preview-only.
    pub dry_run: bool,
    /// Hard dry-run: tracked paths whose local edits would be discarded.
    pub would_overwrite: Vec<String>,
    /// Keep dry-run: paths that would block the reset.
    pub would_block: Vec<String>,
    /// Sample of commits orphaned by the reset (empty when the walk was skipped).
    pub orphaned_commits: Vec<OrphanedCommitSummary>,
    /// Total orphaned commit count (may exceed [`Self::orphaned_commits`].len()).
    pub orphaned_total: usize,
    /// True when the orphan listing was capped by [`ResetOptions::orphan_limit`].
    pub orphaned_truncated: bool,
}

/// Compute reset effects without touching refs, index, or worktree.
///
/// # Errors
///
/// Same validation failures as [`reset_to_commit`] (missing commit, invalid `HEAD`, parse errors).
pub fn preview_reset(repo: &Repository, opts: &ResetOptions) -> Result<ResetPreview> {
    let (_previous, _head, preview) = plan_reset(repo, opts)?;
    Ok(preview)
}

/// Move `HEAD` to `opts.target` according to `opts.mode`.
///
/// Index and worktree updates run before the ref moves so a failure leaves refs unchanged.
/// When [`ResetOptions::dry_run`] is true, nothing is written and the returned
/// [`ResetOutcome::target`] is still the requested commit.
///
/// # Errors
///
/// Returns an error when there is no current commit, the work tree is required but
/// missing (hard/keep reset), local edits block keep reset, or ref/index/checkout I/O fails.
pub fn reset_to_commit(repo: &Repository, opts: &ResetOptions) -> Result<ResetOutcome> {
    let (previous, head, preview) = plan_reset(repo, opts)?;
    let ref_label = ref_label_for_head(&head);

    if opts.dry_run {
        return Ok(ResetOutcome {
            mode: opts.mode,
            target: opts.target,
            previous,
            ref_updated: ref_label,
            dry_run: true,
            would_overwrite: preview.would_overwrite,
            would_block: preview.would_block,
            orphaned_commits: preview.orphaned_commits,
            orphaned_total: preview.orphaned_total,
            orphaned_truncated: preview.orphaned_truncated,
        });
    }

    apply_reset_worktree(repo, opts, previous)?;
    let ref_updated = if previous != opts.target {
        match &head {
            HeadState::Branch { refname, .. } => {
                update_moving_ref(repo, refname, previous, opts.target, opts)?;
                refname.clone()
            }
            HeadState::Detached { .. } => {
                refs::write_ref(repo.git_dir(), "HEAD", &opts.target)?;
                write_head_reflog(repo, previous, opts.target, opts)?;
                "HEAD".to_owned()
            }
            HeadState::Invalid => return Err(Error::InvalidHead),
        }
    } else {
        ref_label
    };

    Ok(ResetOutcome {
        mode: opts.mode,
        target: opts.target,
        previous,
        ref_updated,
        dry_run: false,
        would_overwrite: Vec::new(),
        would_block: Vec::new(),
        orphaned_commits: preview.orphaned_commits,
        orphaned_total: preview.orphaned_total,
        orphaned_truncated: preview.orphaned_truncated,
    })
}

fn plan_reset(
    repo: &Repository,
    opts: &ResetOptions,
) -> Result<(ObjectId, HeadState, ResetPreview)> {
    let head = resolve_head(repo.git_dir())?;
    let previous = head.oid().copied().ok_or(Error::UnbornHead)?;

    let head_tree = commit_tree_oid(repo, &previous)?;
    let target_tree = commit_tree_oid(repo, &opts.target)?;

    let (would_overwrite, would_block) = match opts.mode {
        ResetMode::Hard if repo.work_tree().is_some() => (
            hard_reset_would_discard_paths(repo, Some(head_tree))?,
            Vec::new(),
        ),
        ResetMode::Keep if repo.work_tree().is_some() => (
            Vec::new(),
            keep_reset_would_block_paths(repo, &head_tree, &target_tree)?,
        ),
        ResetMode::Hard | ResetMode::Keep => (Vec::new(), Vec::new()),
        _ => (Vec::new(), Vec::new()),
    };

    let orphan = compute_orphan_info(
        repo,
        &head,
        previous,
        opts.target,
        opts.effective_orphan_limit(),
    )?;

    Ok((
        previous,
        head,
        ResetPreview {
            would_overwrite,
            would_block,
            orphaned_commits: orphan.commits,
            orphaned_total: orphan.total,
            orphaned_truncated: orphan.truncated,
        },
    ))
}

struct OrphanComputation {
    commits: Vec<OrphanedCommitSummary>,
    total: usize,
    truncated: bool,
}

fn compute_orphan_info(
    repo: &Repository,
    head: &HeadState,
    previous: ObjectId,
    target: ObjectId,
    limit: usize,
) -> Result<OrphanComputation> {
    if previous == target || is_ancestor(repo, previous, target)? {
        return Ok(OrphanComputation {
            commits: Vec::new(),
            total: 0,
            truncated: false,
        });
    }
    let hide = orphan_hide_tips(repo, head, target, previous)?;
    let (total, oids) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        std::slice::from_ref(&previous),
        &hide,
        ReachableWalkLimit::CountAllRetainNewest(limit),
    )?;
    let truncated = total > oids.len();
    let mut commits = Vec::with_capacity(oids.len());
    for oid in oids {
        commits.push(orphan_commit_summary(repo, oid)?);
    }
    Ok(OrphanComputation {
        commits,
        total,
        truncated,
    })
}

fn orphan_hide_tips(
    repo: &Repository,
    head: &HeadState,
    target: ObjectId,
    previous: ObjectId,
) -> Result<Vec<ObjectId>> {
    let git_dir = repo.git_dir();
    let updated_branch = match head {
        HeadState::Branch { refname, .. } => Some(refname.as_str()),
        _ => None,
    };
    let mut hide = vec![target];
    for (refname, oid) in refs::list_refs(git_dir, "refs/")? {
        if updated_branch == Some(refname.as_str()) {
            continue;
        }
        if oid != previous {
            hide.push(oid);
        }
    }
    if let Ok(oid) = refs::resolve_ref(git_dir, "refs/stash") {
        hide.push(oid);
    }
    if matches!(head, HeadState::Detached { .. }) {
        hide.push(previous);
    }
    for wt in list_worktrees(repo)? {
        match &wt.head {
            HeadState::Detached { oid } if *oid != previous => hide.push(*oid),
            HeadState::Branch {
                refname,
                oid: Some(oid),
                ..
            } if updated_branch != Some(refname.as_str()) && *oid != previous => hide.push(*oid),
            _ => {}
        }
    }
    hide.sort_unstable();
    hide.dedup();
    Ok(hide)
}

fn orphan_commit_summary(repo: &Repository, oid: ObjectId) -> Result<OrphanedCommitSummary> {
    let obj = repo.odb().read(&oid)?;
    let data = parse_commit(&obj.data)?;
    let subject = read_commit_subject_line(&obj.data).unwrap_or_default();
    let change_id = resolved_change_id(&oid, &data).id.as_str().to_owned();
    Ok(OrphanedCommitSummary {
        oid,
        change_id,
        subject,
    })
}

fn apply_reset_worktree(repo: &Repository, opts: &ResetOptions, previous: ObjectId) -> Result<()> {
    let target_tree = commit_tree_oid(repo, &opts.target)?;
    match opts.mode {
        ResetMode::Soft => {}
        ResetMode::Mixed => {
            let mut index = index_from_tree_with_worktree_stats(repo, &target_tree)?;
            repo.write_index(&mut index)?;
        }
        ResetMode::Hard => {
            if repo.work_tree().is_none() {
                return Err(Error::PathError(
                    "cannot hard reset a bare repository's working tree".into(),
                ));
            }
            let head_tree = commit_tree_oid(repo, &previous)?;
            checkout_hard_to_tree_from_head_tree(repo, &head_tree, &target_tree)?;
        }
        ResetMode::Keep => {
            if repo.work_tree().is_none() {
                return Err(Error::PathError(
                    "cannot keep-reset a bare repository's working tree".into(),
                ));
            }
            let head_tree = commit_tree_oid(repo, &previous)?;
            checkout_tree_transition_allow_local_reset(repo, Some(&head_tree), &target_tree)?;
        }
    }
    Ok(())
}

fn hard_reset_would_discard_paths(
    repo: &Repository,
    head_tree: Option<ObjectId>,
) -> Result<Vec<String>> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let (staged, unstaged, _) = local_change_diffs(repo, &mut snapshot.index, head_tree)?;
    Ok(paths_from_local_diff_entries(&staged, &unstaged))
}

fn paths_from_local_diff_entries(staged: &[DiffEntry], unstaged: &[DiffEntry]) -> Vec<String> {
    let mut paths: Vec<String> = staged
        .iter()
        .chain(unstaged.iter())
        .filter_map(|entry| entry.new_path.clone().or_else(|| entry.old_path.clone()))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn keep_reset_would_block_paths(
    repo: &Repository,
    head_tree: &ObjectId,
    target_tree: &ObjectId,
) -> Result<Vec<String>> {
    match prepare_tree_transition_allow_local(repo, Some(head_tree), target_tree, &HashSet::new()) {
        Ok(_) => Ok(Vec::new()),
        Err(TreeTransitionPrepareError::OverwritePaths(paths)) => Ok(paths),
        Err(TreeTransitionPrepareError::Failed(err)) => Err(err),
    }
}

fn ref_label_for_head(head: &HeadState) -> String {
    match head {
        HeadState::Branch { short_name, .. } => short_name.clone(),
        HeadState::Detached { oid } => format!("HEAD ({})", oid.to_hex()),
        HeadState::Invalid => "HEAD".to_owned(),
    }
}

fn commit_tree_oid(repo: &Repository, commit: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb().read(commit)?;
    match obj.kind {
        ObjectKind::Commit => Ok(parse_commit(&obj.data)?.tree),
        ObjectKind::Tree | ObjectKind::Blob => Err(Error::InvalidRef(format!(
            "object {commit} does not name a commit"
        ))),
        ObjectKind::Tag => Err(Error::InvalidRef("unexpected tag after peel".into())),
    }
}

fn update_moving_ref(
    repo: &Repository,
    refname: &str,
    old: ObjectId,
    new: ObjectId,
    opts: &ResetOptions,
) -> Result<()> {
    refs::write_ref(repo.git_dir(), refname, &new)?;
    write_branch_and_head_reflogs(repo, refname, old, new, opts)
}

fn write_branch_and_head_reflogs(
    repo: &Repository,
    refname: &str,
    old: ObjectId,
    new: ObjectId,
    opts: &ResetOptions,
) -> Result<()> {
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    if refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), refname) {
        append_reflog(
            repo.git_dir(),
            refname,
            &old,
            &new,
            &opts.identity,
            &opts.message,
            false,
        )?;
    }
    if refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), "HEAD") {
        append_reflog(
            repo.git_dir(),
            "HEAD",
            &old,
            &new,
            &opts.identity,
            &opts.message,
            false,
        )?;
    }
    Ok(())
}

fn write_head_reflog(
    repo: &Repository,
    old: ObjectId,
    new: ObjectId,
    opts: &ResetOptions,
) -> Result<()> {
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    if refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), "HEAD") {
        append_reflog(
            repo.git_dir(),
            "HEAD",
            &old,
            &new,
            &opts.identity,
            &opts.message,
            false,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
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

    fn commit_two_files(
        repo: &Repository,
        paths: &[(&str, &str)],
        parent: Option<ObjectId>,
    ) -> ObjectId {
        let wt = repo.work_tree().expect("wt");
        let mut index = repo.load_index().expect("index");
        for (path, contents) in paths {
            std::fs::write(wt.join(path), contents).expect("write");
            let oid = repo
                .odb()
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
        }
        index.sort();
        let tree = write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
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
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "refs/heads/main", &co).expect("branch");
        refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
        co
    }

    fn commit_file(
        repo: &Repository,
        path: &str,
        contents: &str,
        parent: Option<ObjectId>,
    ) -> ObjectId {
        commit_two_files(repo, &[(path, contents)], parent)
    }

    fn reset_opts(target: ObjectId, mode: ResetMode) -> ResetOptions {
        ResetOptions {
            target,
            mode,
            identity: "T <t@t> 0 +0000".into(),
            message: "reset".into(),
            dry_run: false,
            orphan_limit: DEFAULT_ORPHAN_LIMIT,
        }
    }

    #[test]
    fn reset_committish_peels_annotated_tag() {
        use crate::objects::{serialize_tag, TagData};
        use crate::revwalk::rev_parse::resolve_revision_as_commit_without_index_dwim;

        let (_dir, repo) = init_repo();
        let tip = commit_file(&repo, "f", "1\n", None);
        let tag_bytes = serialize_tag(&TagData {
            object: tip,
            object_type: "commit".into(),
            tag: "v1".into(),
            tagger: Some("T <t@t> 0 +0000".into()),
            extra_headers: Vec::new(),
            message: "release\n".into(),
        });
        let tag_oid = repo.odb().write(ObjectKind::Tag, &tag_bytes).expect("tag");
        refs::write_ref(repo.git_dir(), "refs/tags/v1", &tag_oid).expect("tag ref");

        let resolved =
            resolve_revision_as_commit_without_index_dwim(&repo, "v1").expect("peel tag to commit");
        assert_eq!(resolved, tip);
        reset_to_commit(&repo, &reset_opts(resolved, ResetMode::Hard)).expect("reset to tag");
        assert_eq!(
            refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            tip
        );
    }

    #[test]
    fn reset_rejects_tree_committish() {
        use crate::revwalk::rev_parse::resolve_revision;

        let (_dir, repo) = init_repo();
        commit_file(&repo, "f", "1\n", None);
        let tree = resolve_revision(&repo, "HEAD^{tree}").expect("tree");
        let err = reset_to_commit(&repo, &reset_opts(tree, ResetMode::Hard)).expect_err("tree");
        assert!(
            matches!(err, Error::InvalidRef(ref msg) if msg.contains("does not name a commit")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn mixed_reset_moves_tip_and_index_not_worktree() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "2\n").expect("wt");

        let outcome = reset_to_commit(&repo, &reset_opts(c1, ResetMode::Mixed)).expect("reset");

        assert_eq!(outcome.target, c1);
        assert_eq!(outcome.previous, c2);
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c1
        );
        let index = repo.load_index().expect("index");
        let entry = index.get(b"f", 0).expect("indexed");
        let blob = repo.odb().read(&entry.oid).expect("blob");
        assert_eq!(blob.data, b"1\n");
        let disk = std::fs::read(wt.join("f")).expect("disk");
        assert_eq!(disk, b"2\n");
    }

    #[test]
    fn mixed_reset_at_current_head_clears_staged_changes() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "base\n", None);
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "staged\n").expect("wt");
        let mut index = repo.load_index().expect("index");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, b"staged\n")
            .expect("blob");
        let entry =
            crate::index::entry_from_stat(&wt.join("f"), b"f", oid, crate::index::MODE_REGULAR)
                .expect("entry");
        index.add_or_replace(entry);
        repo.write_index(&mut index).expect("write index");

        reset_to_commit(&repo, &reset_opts(c1, ResetMode::Mixed)).expect("reset");

        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(st.staged.is_empty(), "index should match HEAD tree");
    }

    #[test]
    fn mixed_reset_at_head_refreshes_index_stat_for_clean_status() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "a.txt", "a\n", None);
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("b.txt"), "b\n").expect("write b");
        let c2 = commit_two_files(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")], Some(c1));

        reset_to_commit(&repo, &reset_opts(c2, ResetMode::Mixed)).expect("reset at HEAD");

        let index = repo.load_index().expect("index");
        assert!(index.get(b"a.txt", 0).expect("a").size > 0);
        assert!(index.get(b"b.txt", 0).expect("b").size > 0);
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(st.unstaged.is_empty(), "worktree matches HEAD after reset");
        assert!(st.staged.is_empty());
    }

    #[test]
    fn mixed_reset_back_only_lists_paths_that_differ_from_target() {
        let (_dir, repo) = init_repo();
        let c1 = commit_two_files(&repo, &[("a.txt", "v1\n"), ("b.txt", "same\n")], None);
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("a.txt"), "v2\n").expect("edit a");
        let c2 = commit_two_files(&repo, &[("a.txt", "v2\n"), ("b.txt", "same\n")], Some(c1));

        reset_to_commit(&repo, &reset_opts(c1, ResetMode::Mixed)).expect("reset back");

        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        let unstaged: Vec<_> = st.unstaged.iter().map(|e| e.path()).collect();
        assert_eq!(unstaged, ["a.txt"]);
        let _ = c2;
    }

    #[test]
    fn hard_reset_at_current_head_restores_worktree() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "base\n", None);
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "precious\n").expect("wt");

        reset_to_commit(&repo, &reset_opts(c1, ResetMode::Hard)).expect("reset");

        let disk = std::fs::read(wt.join("f")).expect("disk");
        assert_eq!(disk, b"base\n");
    }

    #[test]
    fn hard_reset_dirty_path_unchanged_between_commits() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "base\n", None);
        let c2 = commit_file(&repo, "g", "other\n", Some(c1));
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "precious\n").expect("wt");

        reset_to_commit(&repo, &reset_opts(c1, ResetMode::Hard)).expect("reset");

        assert_eq!(std::fs::read(wt.join("f")).expect("disk"), b"base\n");
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c1
        );
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(st.staged.is_empty());
        assert!(st.unstaged.is_empty());
        let _ = c2;
    }

    #[test]
    fn hard_reset_clears_unmerged_index_stages() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "clean\n", None);
        let wt = repo.work_tree().expect("wt");
        let b1 = repo.odb().write(ObjectKind::Blob, b"base\n").expect("blob");
        let b2 = repo.odb().write(ObjectKind::Blob, b"ours\n").expect("blob");
        let b3 = repo
            .odb()
            .write(ObjectKind::Blob, b"theirs\n")
            .expect("blob");
        let mut index = Index::new();
        for (stage, oid, size) in [(1u8, b1, 5u32), (2, b2, 5), (3, b3, 7)] {
            let mut entry = crate::index::IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: crate::index::MODE_REGULAR,
                uid: 0,
                gid: 0,
                size,
                oid,
                flags: 1,
                flags_extended: None,
                path: b"f".to_vec(),
                base_index_pos: 0,
            };
            entry.set_stage(stage);
            index.add_or_replace(entry);
        }
        index.sort();
        std::fs::write(wt.join("f"), "ours\n").expect("wt");
        repo.write_index(&mut index).expect("write index");

        reset_to_commit(&repo, &reset_opts(c1, ResetMode::Hard)).expect("reset");

        let index = repo.load_index().expect("index");
        assert!(
            index.entries.iter().all(|e| e.stage() == 0),
            "hard reset must drop unmerged stages"
        );
        assert_eq!(index.entries.len(), 1);
        let blob = repo
            .odb()
            .read(&index.get(b"f", 0).expect("entry").oid)
            .expect("blob");
        assert_eq!(blob.data, b"clean\n");
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(st.staged.is_empty());
        assert!(st.unstaged.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hard_reset_at_head_reverts_exec_bit_only_changes() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("plain.txt"), "hi\n").expect("write");
        std::fs::write(wt.join("run.sh"), "#!/bin/sh\n").expect("write");
        let mut perms = std::fs::metadata(wt.join("run.sh"))
            .expect("meta")
            .permissions();
        perms.set_mode(0o100755);
        std::fs::set_permissions(wt.join("run.sh"), perms).expect("chmod");
        let c1 = {
            let mut index = repo.load_index().expect("index");
            for (path, contents, mode) in [
                ("plain.txt", "hi\n", crate::index::MODE_REGULAR),
                ("run.sh", "#!/bin/sh\n", crate::index::MODE_EXECUTABLE),
            ] {
                let oid = repo
                    .odb()
                    .write(ObjectKind::Blob, contents.as_bytes())
                    .expect("blob");
                let entry =
                    crate::index::entry_from_stat(&wt.join(path), path.as_bytes(), oid, mode)
                        .expect("entry");
                index.add_or_replace(entry);
            }
            index.sort();
            let tree =
                write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                    .expect("tree");
            repo.write_index(&mut index).expect("write index");
            let commit = CommitData {
                tree,
                parents: vec![],
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
                .odb()
                .write(ObjectKind::Commit, &serialize_commit(&commit))
                .expect("commit");
            refs::write_ref(&repo.git_dir(), "refs/heads/main", &co).expect("branch");
            refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
            co
        };

        let mut plain = std::fs::metadata(wt.join("plain.txt"))
            .expect("meta")
            .permissions();
        plain.set_mode(0o100755);
        std::fs::set_permissions(wt.join("plain.txt"), plain).expect("chmod plain");
        let mut run = std::fs::metadata(wt.join("run.sh"))
            .expect("meta")
            .permissions();
        run.set_mode(0o100644);
        std::fs::set_permissions(wt.join("run.sh"), run).expect("chmod run");

        reset_to_commit(&repo, &reset_opts(c1, ResetMode::Hard)).expect("reset");

        assert_eq!(
            std::fs::metadata(wt.join("plain.txt"))
                .expect("meta")
                .permissions()
                .mode()
                & 0o111,
            0,
            "plain.txt should not be executable after hard reset"
        );
        assert_ne!(
            std::fs::metadata(wt.join("run.sh"))
                .expect("meta")
                .permissions()
                .mode()
                & 0o111,
            0,
            "run.sh should be executable after hard reset"
        );
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(st.staged.is_empty(), "{st:?}");
        assert!(st.unstaged.is_empty(), "{st:?}");
    }

    #[test]
    fn bare_hard_reset_fails_without_moving_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo =
            init_repository(dir.path(), true, "main", None, RefStorageFormat::Files).expect("init");
        let mut index = Index::new();
        let tree = write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        let commit = CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "c\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let c1 = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "refs/heads/main", &c1).expect("branch");
        refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");

        let err = reset_to_commit(&repo, &reset_opts(c1, ResetMode::Hard)).expect_err("bare");
        assert!(matches!(err, Error::PathError(_)));
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c1
        );
    }

    #[test]
    fn dry_run_hard_leaves_repo_unchanged() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "dirty\n").expect("dirty");

        let mut opts = reset_opts(c1, ResetMode::Hard);
        opts.dry_run = true;
        let outcome = reset_to_commit(&repo, &opts).expect("dry-run");
        assert!(outcome.dry_run);
        assert!(outcome.would_overwrite.iter().any(|p| p == "f"));
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c2
        );
        assert_eq!(std::fs::read(wt.join("f")).expect("disk"), b"dirty\n");
    }

    #[test]
    fn orphan_walk_lists_commit_not_held_by_other_ref() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let c3 = commit_file(&repo, "f", "3\n", Some(c2));
        refs::write_ref(&repo.git_dir(), "refs/heads/keep", &c2).expect("keep");

        let preview = preview_reset(
            &repo,
            &reset_opts(c1, ResetMode::Mixed),
        )
        .expect("preview");
        assert_eq!(preview.orphaned_total, 1, "c3 should orphan when keep stops at c2");
        assert_eq!(preview.orphaned_commits[0].oid, c3);
    }

    #[test]
    fn forward_reset_skips_orphan_walk() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let outcome = reset_to_commit(&repo, &reset_opts(c2, ResetMode::Mixed)).expect("reset");
        assert_eq!(outcome.orphaned_total, 0);
        assert!(outcome.orphaned_commits.is_empty());
    }
}
