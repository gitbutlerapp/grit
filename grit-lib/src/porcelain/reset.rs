//! Move `HEAD` and optionally the index and working tree to a target commit.

use crate::config::ConfigSet;
use crate::diff::DiffEntry;
use crate::error::{Error, Result};
use crate::ident::Signature;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use crate::porcelain::index_from_tree::index_from_tree_with_owned_seed_worktree_stats;
use crate::porcelain::orphans::compute_orphan_info;
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local_reset, keep_style_transition_would_block_paths_for_preview,
};
use crate::porcelain::worktree_guard::{load_worktree_snapshot, local_change_diffs};
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags, ReflogUpdate};
use crate::repo::Repository;
use crate::state::{clear_merge_state, clear_replay_state, resolve_head_for_repository, HeadState};

pub use crate::porcelain::orphans::{OrphanedCommitSummary, DEFAULT_ORPHAN_LIMIT};

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

/// Inputs for [`reset_to_commit`] and [`preview_reset`].
#[derive(Debug, Clone)]
pub struct ResetOptions {
    /// Commit to reset to.
    pub target: ObjectId,
    /// Soft, mixed (default), hard, or keep reset.
    pub mode: ResetMode,
    /// Committer recorded on reset reflog entries (name, email, and timestamp).
    pub identity: Signature,
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
        identity: Signature,
        message: impl Into<String>,
    ) -> Self {
        Self {
            target,
            mode,
            identity,
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
    /// Advisory: without a commit-graph, date-based stopping can miscount under severe
    /// clock skew. The report does not control repository mutations.
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
    /// Advisory: without a commit-graph, date-based stopping can miscount under severe
    /// clock skew. The report does not control repository mutations.
    pub orphaned_total: usize,
    /// True when the orphan listing was capped by [`ResetOptions::orphan_limit`].
    pub orphaned_truncated: bool,
}

/// Compute reset effects without touching refs, index, or worktree.
///
/// # Errors
///
/// Same validation failures as [`reset_to_commit`], including [`Error::InvalidSignature`]
/// for an invalid [`ResetOptions::identity`], even when the target is already `HEAD`
/// or reflog updates are disabled. No repository state is changed.
pub fn preview_reset(repo: &Repository, opts: &ResetOptions) -> Result<ResetPreview> {
    let (_previous, _head, preview) = plan_reset(repo, opts, true)?;
    Ok(preview)
}

/// Move `HEAD` to `opts.target` according to `opts.mode`.
///
/// Index and worktree updates run before the branch or detached `HEAD` moves.
/// Hard and keep resets take `index.lock` before reading the index or touching the
/// worktree. When [`ResetOptions::dry_run`] is true, nothing is written and the returned
/// [`ResetOutcome::target`] is still the requested commit.
///
/// # Errors
///
/// Returns [`Error::LockHeld`] when another process holds `index.lock`, with `HEAD`,
/// refs, index and worktree unchanged. Returns an error when there is no current commit,
/// the work tree is required but missing (hard/keep reset), local edits block keep reset,
/// or ref/index/checkout I/O fails.
/// An invalid [`ResetOptions::identity`] returns [`Error::InvalidSignature`] before
/// changing HEAD, branch refs, index, worktree, reflogs, or operation state (including
/// merge autostashes). This also applies to dry runs, resets to the current HEAD,
/// and resets with reflog updates disabled. Later I/O failures may leave partial updates.
pub fn reset_to_commit(
    repo: &Repository,
    opts: &ResetOptions,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<ResetOutcome> {
    let (previous, head, preview) = plan_reset(repo, opts, opts.dry_run)?;
    let ref_label = ref_label_for_head(&head);

    if opts.dry_run {
        return Ok(ResetOutcome {
            mode: opts.mode,
            target: opts.target,
            previous,
            ref_updated: ref_updated_label_after_reset(&head),
            dry_run: true,
            would_overwrite: preview.would_overwrite,
            would_block: preview.would_block,
            orphaned_commits: preview.orphaned_commits,
            orphaned_total: preview.orphaned_total,
            orphaned_truncated: preview.orphaned_truncated,
        });
    }

    apply_reset_worktree(repo, opts, previous, progress, cancel)?;
    crate::porcelain::stash::save_merge_autostash_ref(repo, &opts.identity.header_line())?;
    clear_merge_state(repo.git_dir())?;
    clear_replay_state(repo.git_dir())?;
    let ref_updated = if previous != opts.target {
        match &head {
            HeadState::Branch { refname, .. } => {
                update_moving_ref(repo, refname, previous, opts.target, opts)?;
                refname.clone()
            }
            HeadState::Detached { .. } => {
                refs::update_direct_ref_on_store(
                    repo.refs(),
                    repo.git_dir(),
                    "HEAD",
                    &opts.target,
                )?;
                append_reset_reflogs(repo, &["HEAD"], previous, opts.target, opts)?;
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

/// Resolve the reset endpoints and orphan summary.
///
/// `with_worktree_preview` computes the paths a hard reset would discard (a full worktree
/// diff) and the paths that block a keep reset. A real reset skips the hard-reset scan: it
/// discards those edits anyway, and the keep checkout runs its own guard.
fn plan_reset(
    repo: &Repository,
    opts: &ResetOptions,
    with_worktree_preview: bool,
) -> Result<(ObjectId, HeadState, ResetPreview)> {
    opts.identity.validate().map_err(Error::InvalidSignature)?;
    let head = resolve_head_for_repository(repo)?;
    let previous = head.oid().copied().ok_or(Error::UnbornHead)?;

    ensure_reset_worktree_mode(repo, opts.mode)?;
    ensure_keep_index_merged(repo, opts.mode)?;

    let head_tree = commit_tree_oid(repo, &previous)?;
    let target_tree = commit_tree_oid(repo, &opts.target)?;

    let (would_overwrite, would_block) = match opts.mode {
        _ if !with_worktree_preview => (Vec::new(), Vec::new()),
        ResetMode::Hard => (
            hard_reset_would_discard_paths(repo, Some(head_tree))?,
            Vec::new(),
        ),
        ResetMode::Keep => (
            Vec::new(),
            keep_reset_would_block_paths(repo, &head_tree, &target_tree)?,
        ),
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

fn apply_reset_worktree(
    repo: &Repository,
    opts: &ResetOptions,
    previous: ObjectId,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    let target_tree = commit_tree_oid(repo, &opts.target)?;
    match opts.mode {
        ResetMode::Soft => {}
        ResetMode::Mixed => {
            // The current index only seeds cached stat data, so an unreadable one must not
            // block the reset that replaces it.
            let current = repo.load_index().unwrap_or_default();
            let mut index =
                index_from_tree_with_owned_seed_worktree_stats(repo, &target_tree, current)?;
            repo.write_index(&mut index)?;
        }
        ResetMode::Hard => {
            ensure_reset_worktree_mode(repo, ResetMode::Hard)?;
            let head_tree = commit_tree_oid(repo, &previous)?;
            checkout_hard_to_tree_from_head_tree(repo, &head_tree, &target_tree, progress, cancel)?;
        }
        ResetMode::Keep => {
            ensure_reset_worktree_mode(repo, ResetMode::Keep)?;
            ensure_keep_index_merged(repo, opts.mode)?;
            let head_tree = commit_tree_oid(repo, &previous)?;
            checkout_tree_transition_allow_local_reset(
                repo,
                Some(&head_tree),
                &target_tree,
                progress,
                cancel,
            )?;
        }
    }
    Ok(())
}

fn ensure_reset_worktree_mode(repo: &Repository, mode: ResetMode) -> Result<()> {
    if repo.work_tree().is_some() {
        return Ok(());
    }
    match mode {
        ResetMode::Hard => Err(Error::PathError(
            "cannot hard reset a bare repository's working tree".into(),
        )),
        ResetMode::Keep => Err(Error::PathError(
            "cannot keep-reset a bare repository's working tree".into(),
        )),
        _ => Ok(()),
    }
}

fn ensure_keep_index_merged(repo: &Repository, mode: ResetMode) -> Result<()> {
    if mode == ResetMode::Keep && repo.load_index()?.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
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
    keep_style_transition_would_block_paths_for_preview(repo, Some(head_tree), target_tree)
}

fn ref_updated_label_after_reset(head: &HeadState) -> String {
    match head {
        HeadState::Branch { refname, .. } => refname.clone(),
        HeadState::Detached { .. } | HeadState::Invalid => "HEAD".to_owned(),
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
    refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), refname, &new)?;
    append_reset_reflogs(repo, &[refname, "HEAD"], old, new, opts)
}

/// Append `old -> new` to each of `names`' reflogs where `core.logAllRefUpdates` logs it.
fn append_reset_reflogs(
    repo: &Repository,
    names: &[&str],
    old: ObjectId,
    new: ObjectId,
    opts: &ResetOptions,
) -> Result<()> {
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    let log = ReflogUpdate {
        identity: opts.identity.reflog_name_email(),
        message: opts.message.clone(),
        time: opts.identity.time,
        old_oid: Some(old),
        new_oid: Some(new),
    };
    for name in names {
        if refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), name) {
            refs::commit_ref_update_on_store(
                repo.refs(),
                RefUpdate {
                    name: (*name).to_owned(),
                    new_value: Some(RawRef::Direct(new)),
                    expected: Expected::Any,
                    reflog: Some(log.clone()),
                    flags: RefUpdateFlags {
                        no_deref: true,
                        log_only: true,
                    },
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use crate::objects::{serialize_commit, CommitData, ObjectKind};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::{NeverCancel, NullProgress};
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
            identity: "T <t@t> 0 +0000".parse().expect("identity"),
            message: "reset".into(),
            dry_run: false,
            orphan_limit: DEFAULT_ORPHAN_LIMIT,
        }
    }

    #[test]
    fn invalid_signature_fails_shared_preflight_at_current_head() {
        let (_dir, repo) = init_repo();
        let head = commit_file(&repo, "f", "base\n", None);
        let mut opts = reset_opts(head, ResetMode::Hard);
        opts.identity.email.push('\n');
        for with_worktree_preview in [false, true] {
            let error = plan_reset(&repo, &opts, with_worktree_preview).unwrap_err();
            assert!(matches!(
                error,
                Error::InvalidSignature(crate::ident::SignatureError::ForbiddenCharacter {
                    field: crate::ident::SignatureField::Email,
                    ch: '\n',
                })
            ));
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
        reset_to_commit(
            &repo,
            &reset_opts(resolved, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset to tag");
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
        let err = reset_to_commit(
            &repo,
            &reset_opts(tree, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("tree");
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

        let outcome = reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

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

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
        assert!(st.staged.is_empty(), "index should match HEAD tree");
    }

    #[test]
    fn mixed_reset_at_head_refreshes_index_stat_for_clean_status() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "a.txt", "a\n", None);
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("b.txt"), "b\n").expect("write b");
        let c2 = commit_two_files(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")], Some(c1));

        reset_to_commit(
            &repo,
            &reset_opts(c2, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset at HEAD");

        let index = repo.load_index().expect("index");
        assert!(index.get(b"a.txt", 0).expect("a").size > 0);
        assert!(index.get(b"b.txt", 0).expect("b").size > 0);
        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
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

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset back");

        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
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

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

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

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

        assert_eq!(std::fs::read(wt.join("f")).expect("disk"), b"base\n");
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c1
        );
        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
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

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

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
        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
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

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

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
        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
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

        let err = reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("bare");
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
        let outcome = reset_to_commit(
            &repo,
            &opts,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("dry-run");
        assert!(outcome.dry_run);
        assert!(outcome.would_overwrite.iter().any(|p| p == "f"));
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c2
        );
        assert_eq!(std::fs::read(wt.join("f")).expect("disk"), b"dirty\n");
    }

    #[test]
    fn keep_dry_run_lists_local_and_untracked_blockers() {
        let (_dir, repo) = init_repo();
        let c1 = commit_two_files(&repo, &[("f", "1\n"), ("u", "base\n")], None);
        let wt = repo.work_tree().expect("wt");
        {
            let mut index = repo.load_index().expect("index");
            index.remove(b"u");
            std::fs::write(wt.join("f"), "2\n").expect("f at tip");
            let oid_f = repo.odb().write(ObjectKind::Blob, b"2\n").expect("blob f");
            let entry_f = crate::index::entry_from_stat(
                &wt.join("f"),
                b"f",
                oid_f,
                crate::index::MODE_REGULAR,
            )
            .expect("entry f");
            index.add_or_replace(entry_f);
            index.sort();
            let tree =
                write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                    .expect("tree");
            repo.write_index(&mut index).expect("write index");
            let commit = CommitData {
                tree,
                parents: vec![c1],
                author: "T <t@t> 0 +0000".into(),
                committer: "T <t@t> 0 +0000".into(),
                author_raw: vec![],
                committer_raw: vec![],
                encoding: None,
                message: "drop u\n".into(),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            let c2 = repo
                .odb()
                .write(ObjectKind::Commit, &serialize_commit(&commit))
                .expect("commit");
            refs::write_ref(&repo.git_dir(), "refs/heads/main", &c2).expect("branch");
            refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
            let _ = c2;
        }

        std::fs::write(wt.join("f"), "local\n").expect("local edit on f");
        std::fs::write(wt.join("u"), "untracked\n").expect("untracked u");

        let mut opts = reset_opts(c1, ResetMode::Keep);
        opts.dry_run = true;
        let outcome = reset_to_commit(
            &repo,
            &opts,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("keep dry-run");
        assert_eq!(outcome.would_block, ["f", "u"]);
    }

    #[test]
    fn orphan_zero_when_other_ref_holds_previous_tip() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let c3 = commit_file(&repo, "f", "3\n", Some(c2));
        refs::write_ref(&repo.git_dir(), "refs/heads/hold", &c3).expect("hold");

        let preview = preview_reset(&repo, &reset_opts(c1, ResetMode::Mixed)).expect("preview");
        assert_eq!(
            preview.orphaned_total, 0,
            "tip still held by refs/heads/hold"
        );
        let _ = c3;
    }

    #[test]
    fn reset_orphan_walk_ignores_tag_peeled_to_blob() {
        use crate::objects::{serialize_tag, TagData};

        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let blob = repo.odb().write(ObjectKind::Blob, b"pubkey").expect("blob");
        let tag_bytes = serialize_tag(&TagData {
            object: blob,
            object_type: "blob".into(),
            tag: "keytag".into(),
            tagger: Some("T <t@t> 0 +0000".into()),
            message: "key\n".into(),
            extra_headers: Vec::new(),
        });
        let tag_oid = repo.odb().write(ObjectKind::Tag, &tag_bytes).expect("tag");
        refs::write_ref(&repo.git_dir(), "refs/tags/keytag", &tag_oid).expect("tag ref");

        for mode in [ResetMode::Soft, ResetMode::Mixed, ResetMode::Hard] {
            refs::write_ref(&repo.git_dir(), "refs/heads/main", &c2).expect("restore tip");
            refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
            reset_to_commit(
                &repo,
                &reset_opts(c1, mode),
                &mut crate::progress::NullProgress,
                &crate::progress::NeverCancel,
            )
            .expect("reset");
            assert_eq!(
                refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
                c1,
                "{mode:?}"
            );
        }

        let mut opts = reset_opts(c1, ResetMode::Mixed);
        opts.dry_run = true;
        refs::write_ref(&repo.git_dir(), "refs/heads/main", &c2).expect("restore tip");
        preview_reset(&repo, &opts).expect("dry-run preview");
        assert_eq!(
            refs::resolve_ref(&repo.git_dir(), "refs/heads/main").expect("ref"),
            c2
        );
    }

    #[test]
    fn orphan_lists_commits_for_detached_reset_back() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let c3 = commit_file(&repo, "f", "3\n", Some(c2));
        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Soft),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("move main back");
        refs::write_ref(&repo.git_dir(), "HEAD", &c3).expect("detach at c3");

        let preview = preview_reset(&repo, &reset_opts(c1, ResetMode::Mixed)).expect("preview");
        assert_eq!(preview.orphaned_total, 2);
        assert_eq!(preview.orphaned_commits.len(), 2);
        assert_eq!(preview.orphaned_commits[0].oid, c3);
        assert_eq!(preview.orphaned_commits[1].oid, c2);
    }

    #[test]
    fn keep_reset_rejects_unmerged_index() {
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

        let err = reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Keep),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("keep");
        assert!(matches!(err, Error::IndexUnmerged));
    }

    #[test]
    fn bare_hard_reset_dry_run_fails_without_moving_head() {
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

        let mut opts = reset_opts(c1, ResetMode::Hard);
        opts.dry_run = true;
        let err = reset_to_commit(
            &repo,
            &opts,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("bare dry-run hard");
        assert!(matches!(err, Error::PathError(_)));
    }

    #[test]
    fn orphan_walk_lists_commit_not_held_by_other_ref() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let c3 = commit_file(&repo, "f", "3\n", Some(c2));
        refs::write_ref(&repo.git_dir(), "refs/heads/keep", &c2).expect("keep");

        let preview = preview_reset(&repo, &reset_opts(c1, ResetMode::Mixed)).expect("preview");
        assert_eq!(
            preview.orphaned_total, 1,
            "c3 should orphan when keep stops at c2"
        );
        assert_eq!(preview.orphaned_commits[0].oid, c3);
    }

    #[test]
    fn forward_reset_skips_orphan_walk() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let c2 = commit_file(&repo, "f", "2\n", Some(c1));
        let outcome = reset_to_commit(
            &repo,
            &reset_opts(c2, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");
        assert_eq!(outcome.orphaned_total, 0);
        assert!(outcome.orphaned_commits.is_empty());
    }

    #[test]
    fn orphan_count_matches_git_rev_list_with_many_tags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main", "."]);
        for i in 1..=120 {
            git(
                root,
                &["commit", "-q", "--allow-empty", "-m", &format!("c{i}")],
            );
            if i % 10 == 0 {
                git(root, &["tag", "-a", "-m", "release", &format!("v{i}")]);
            }
            if i == 100 {
                git(root, &["branch", "side"]);
            }
        }
        git(root, &["tag", "-a", "-m", "outer", "outer", "v10"]);
        let blob = git(root, &["hash-object", "-w", "--stdin"]);
        git(root, &["tag", "-a", "-m", "key", "key", blob.trim()]);
        git(root, &["switch", "-q", "side"]);
        git(root, &["commit", "-q", "--allow-empty", "-m", "side work"]);
        git(root, &["switch", "-q", "main"]);
        let base = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
        for i in 1..=3 {
            git(
                root,
                &["commit", "-q", "--allow-empty", "-m", &format!("local{i}")],
            );
            if i == 1 {
                git(root, &["tag", "-a", "-m", "keep", "keep-local1"]);
            }
        }
        let target = ObjectId::from_hex(&base).expect("oid");
        let opts = reset_opts(target, ResetMode::Mixed);

        for (packed, graph) in [(false, false), (true, false), (true, true)] {
            if packed {
                git(root, &["pack-refs", "--all"]);
            }
            if graph {
                git(root, &["commit-graph", "write", "--reachable"]);
            }
            let repo = Repository::open(
                &root.join(".git"),
                &crate::environment::RepositoryOptions::empty().with_work_tree(root),
            )
            .expect("open");
            let preview = preview_reset(&repo, &opts).expect("preview");
            let mut expected: Vec<ObjectId> = git(
                root,
                &[
                    "rev-list",
                    "main",
                    "--not",
                    "--exclude=main",
                    "--branches",
                    "--tags",
                    &base,
                ],
            )
            .lines()
            .map(|l| ObjectId::from_hex(l).expect("oid"))
            .collect();
            assert_eq!(expected.len(), 2, "local2 and local3 are only on main");
            assert_eq!(
                preview.orphaned_total,
                expected.len(),
                "packed={packed} graph={graph}"
            );
            let mut got: Vec<ObjectId> = preview.orphaned_commits.iter().map(|c| c.oid).collect();
            got.sort_unstable();
            expected.sort_unstable();
            assert_eq!(got, expected, "packed={packed} graph={graph}");
        }
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    const WIDE_DIRS: usize = 24;
    const WIDE_FILES_PER_DIR: usize = 25;
    const WIDE_DELETED_DIRS: usize = 8;

    /// A wide tree on `main` plus branch `trimmed` that deletes the first
    /// [`WIDE_DELETED_DIRS`] top-level directories, built with the system `git`.
    /// Returns the open repository (on `main`) and the `trimmed` tip.
    fn wide_repo_with_trimmed_branch(root: &std::path::Path) -> (Repository, ObjectId) {
        git(root, &["init", "-q", "-b", "main", "."]);
        for d in 0..WIDE_DIRS {
            let dir = root.join(format!("d{d:02}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..WIDE_FILES_PER_DIR {
                std::fs::write(
                    dir.join(format!("f{f:02}.txt")),
                    format!("content {d} {f}\n"),
                )
                .expect("write");
            }
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "full"]);
        git(root, &["switch", "-qc", "trimmed"]);
        let doomed: Vec<String> = (0..WIDE_DELETED_DIRS).map(|d| format!("d{d:02}")).collect();
        let mut rm = vec!["rm", "-rq"];
        rm.extend(doomed.iter().map(String::as_str));
        git(root, &rm);
        git(root, &["commit", "-qm", "trim"]);
        let trimmed = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("oid");
        git(root, &["switch", "-q", "main"]);
        let repo = Repository::open(
            &root.join(".git"),
            &crate::environment::RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open");
        (repo, trimmed)
    }

    /// Pin the mtime of every tracked file to `secs` and let `git` refresh the index, so stat data
    /// is clean and no entry is racy (the index is written later than `secs`).
    fn pin_worktree_mtimes_and_refresh(root: &std::path::Path, secs: i64) {
        let pinned = filetime::FileTime::from_unix_time(secs, 0);
        for d in 0..WIDE_DIRS {
            for f in 0..WIDE_FILES_PER_DIR {
                let path = root.join(format!("d{d:02}/f{f:02}.txt"));
                filetime::set_file_mtime(&path, pinned).expect("pin mtime");
            }
        }
        git(root, &["update-index", "-q", "--refresh"]);
    }

    fn assert_git_sees_clean_repo(root: &std::path::Path) {
        assert_eq!(
            git(root, &["status", "--porcelain"]),
            "",
            "git status clean"
        );
        git(root, &["fsck", "--no-progress"]);
    }

    const ALL_MODIFIED_DIRS: usize = 100;
    const ALL_MODIFIED_FILES_PER_DIR: usize = 50;

    fn wide_repo_two_commits_all_paths_differ(
        root: &std::path::Path,
    ) -> (Repository, ObjectId, ObjectId) {
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "gc.auto", "0"]);
        for d in 0..ALL_MODIFIED_DIRS {
            let dir = root.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..ALL_MODIFIED_FILES_PER_DIR {
                std::fs::write(dir.join(format!("f{f}.txt")), format!("line a {d} {f}\n"))
                    .expect("write");
            }
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        for d in 0..ALL_MODIFIED_DIRS {
            for f in 0..ALL_MODIFIED_FILES_PER_DIR {
                let path = root.join(format!("d{d}/f{f}.txt"));
                std::fs::write(&path, format!("line a {d} {f}\nedit\n")).expect("edit");
            }
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "edited"]);
        let older = ObjectId::from_hex(git(root, &["rev-parse", "HEAD~1"]).trim()).expect("older");
        let newer = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("newer");
        let repo = Repository::open(
            &root.join(".git"),
            &crate::environment::RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open");
        (repo, older, newer)
    }

    #[test]
    fn hard_reset_all_modified_tree_diff_skips_redundant_worktree_stat_scan() {
        use crate::worktree_scan::{reset_worktree_metadata_probe, worktree_metadata_probe_count};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repo, older, newer) = wide_repo_two_commits_all_paths_differ(root);
        let pinned = filetime::FileTime::from_unix_time(1_700_000_100, 0);
        for d in 0..ALL_MODIFIED_DIRS {
            for f in 0..ALL_MODIFIED_FILES_PER_DIR {
                let path = root.join(format!("d{d}/f{f}.txt"));
                filetime::set_file_mtime(&path, pinned).expect("pin mtime");
            }
        }
        git(root, &["update-index", "-q", "--refresh"]);

        let metrics = repo.odb().hot_path_metrics();
        metrics.reset_blob_content_reads();
        metrics.set_blob_counting(true);
        let probes = reset_worktree_metadata_probe(root);
        reset_to_commit(
            &repo,
            &reset_opts(older, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset to older");
        let probe_after_first = worktree_metadata_probe_count();
        reset_to_commit(
            &repo,
            &reset_opts(newer, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset to newer");
        let probe_count = worktree_metadata_probe_count();
        drop(probes);
        metrics.set_blob_counting(false);

        let total_files = ALL_MODIFIED_DIRS * ALL_MODIFIED_FILES_PER_DIR;
        assert_eq!(
            metrics.blob_content_reads(),
            0,
            "tree-diff hard reset must not re-read blob content for every modified path"
        );
        assert!(
            probe_count <= probe_after_first + ALL_MODIFIED_DIRS + 8,
            "tree-diff hard reset should not re-stat every rewritten path ({total_files} files); \
             got {probe_count} metadata probes after the second reset (baseline after first {probe_after_first})"
        );
        let index_path = repo.index_path_for_env().expect("index path");
        let index = repo.load_index().expect("index");
        let (max_sec, max_nsec) = crate::index::stage0_mtime_max(&index.entries);
        let index_meta = std::fs::metadata(&index_path).expect("index meta");
        let index_mtime = filetime::FileTime::from_last_modification_time(&index_meta);
        let index_secs = index_mtime.seconds().try_into().unwrap_or(u32::MAX);
        assert!(
            index_secs > max_sec || (index_secs == max_sec && index_mtime.nanoseconds() > max_nsec),
            "hard reset must advance index mtime past newest entry (index {index_secs}.{} vs entry {max_sec}.{max_nsec})",
            index_mtime.nanoseconds()
        );
        assert_git_sees_clean_repo(root);
    }

    #[test]
    fn hard_reset_delete_heavy_reads_no_worktree_files_and_stats_each_once() {
        use crate::worktree_scan::{reset_worktree_metadata_probe, worktree_metadata_probe_count};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repo, trimmed) = wide_repo_with_trimmed_branch(root);
        pin_worktree_mtimes_and_refresh(root, 1_700_000_000);
        // Same-size rewrites that only bump mtime: a content check would have to read them,
        // but a hard reset deletes these files anyway.
        let bumped = filetime::FileTime::from_unix_time(1_700_000_100, 0);
        for d in 0..WIDE_DELETED_DIRS {
            for f in 0..WIDE_FILES_PER_DIR {
                let path = root.join(format!("d{d:02}/f{f:02}.txt"));
                filetime::set_file_mtime(&path, bumped).expect("bump mtime");
            }
        }

        let metrics = repo.odb().hot_path_metrics();
        metrics.reset_blob_content_reads();
        metrics.set_blob_counting(true);
        let probes = reset_worktree_metadata_probe(root);
        reset_to_commit(
            &repo,
            &reset_opts(trimmed, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset");
        let probe_count = worktree_metadata_probe_count();
        drop(probes);
        metrics.set_blob_counting(false);

        assert_eq!(
            metrics.blob_content_reads(),
            0,
            "hard reset must not read deleted or stat-clean worktree files"
        );
        let kept = (WIDE_DIRS - WIDE_DELETED_DIRS) * WIDE_FILES_PER_DIR;
        assert!(
            probe_count <= kept + WIDE_DIRS + 8,
            "expected about one metadata probe per kept file ({kept}), got {probe_count}"
        );
        for d in 0..WIDE_DELETED_DIRS {
            assert!(!root.join(format!("d{d:02}")).exists(), "d{d:02} removed");
        }
        assert_git_sees_clean_repo(root);
        assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), trimmed.to_hex());
    }

    /// `wide_repo_with_trimmed_branch` with settled stat data, one staged edit, one unstaged
    /// edit and one untracked file.
    fn wide_repo_with_local_changes(root: &std::path::Path) -> (Repository, ObjectId) {
        let (repo, trimmed) = wide_repo_with_trimmed_branch(root);
        pin_worktree_mtimes_and_refresh(root, 1_700_000_000);
        std::fs::write(root.join("d10/f00.txt"), "staged edit\n").expect("staged edit");
        git(root, &["add", "d10/f00.txt"]);
        std::fs::write(root.join("d11/f01.txt"), "unstaged edit\n").expect("unstaged edit");
        std::fs::write(root.join("untracked.txt"), "new\n").expect("untracked");
        (repo, trimmed)
    }

    /// Index rows, status and the stat-only `diff-files` view, as the system `git` sees them.
    fn git_index_view(root: &std::path::Path) -> [String; 3] {
        [
            git(root, &["ls-files", "--stage"]),
            git(root, &["status", "--porcelain"]),
            git(root, &["diff-files", "--name-status"]),
        ]
    }

    #[test]
    fn mixed_reset_at_head_reads_only_stat_dirty_files_and_matches_git() {
        let grit_dir = tempfile::tempdir().expect("tempdir");
        let git_dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = wide_repo_with_local_changes(grit_dir.path());
        wide_repo_with_local_changes(git_dir.path());
        let head =
            ObjectId::from_hex(git(grit_dir.path(), &["rev-parse", "HEAD"]).trim()).expect("head");

        git(git_dir.path(), &["reset", "-q", "--mixed", "HEAD"]);
        let metrics = repo.odb().hot_path_metrics();
        metrics.reset_blob_content_reads();
        metrics.set_blob_counting(true);
        reset_to_commit(
            &repo,
            &reset_opts(head, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("mixed reset");
        metrics.set_blob_counting(false);

        assert!(
            metrics.blob_content_reads() <= 2,
            "only the staged and the edited file need hashing, read {}",
            metrics.blob_content_reads()
        );
        assert_eq!(
            git_index_view(grit_dir.path()),
            git_index_view(git_dir.path())
        );
        assert_eq!(
            git(grit_dir.path(), &["diff-files", "--name-only"]),
            "d10/f00.txt\nd11/f01.txt\n"
        );
        git(grit_dir.path(), &["fsck", "--no-progress"]);
    }

    #[test]
    fn mixed_reset_to_other_commit_matches_git() {
        let grit_dir = tempfile::tempdir().expect("tempdir");
        let git_dir = tempfile::tempdir().expect("tempdir");
        let (repo, trimmed) = wide_repo_with_local_changes(grit_dir.path());
        let (_, git_trimmed) = wide_repo_with_local_changes(git_dir.path());

        git(
            git_dir.path(),
            &["reset", "-q", "--mixed", &git_trimmed.to_hex()],
        );
        reset_to_commit(
            &repo,
            &reset_opts(trimmed, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("mixed reset");

        assert_eq!(
            git_index_view(grit_dir.path()),
            git_index_view(git_dir.path())
        );
        assert_eq!(
            git(grit_dir.path(), &["rev-parse", "HEAD"]).trim(),
            trimmed.to_hex()
        );
        git(grit_dir.path(), &["fsck", "--no-progress"]);
    }

    #[test]
    fn hard_reset_records_the_target_cache_tree_like_git() {
        let grit_dir = tempfile::tempdir().expect("tempdir");
        let git_dir = tempfile::tempdir().expect("tempdir");
        let (repo, trimmed) = wide_repo_with_local_changes(grit_dir.path());
        let (_, git_trimmed) = wide_repo_with_local_changes(git_dir.path());

        git(
            git_dir.path(),
            &["reset", "-q", "--hard", &git_trimmed.to_hex()],
        );
        reset_to_commit(
            &repo,
            &reset_opts(trimmed, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset");

        let tree = commit_tree_oid(&repo, &trimmed).expect("tree");
        assert_eq!(
            repo.load_index().expect("index").cache_tree_root,
            Some(tree)
        );
        assert_eq!(git(grit_dir.path(), &["write-tree"]).trim(), tree.to_hex());
        assert_eq!(
            git_index_view(grit_dir.path()),
            git_index_view(git_dir.path())
        );
        git(grit_dir.path(), &["fsck", "--no-progress"]);
    }

    #[test]
    fn mixed_reset_replaces_an_unreadable_index() {
        let (_dir, repo) = init_repo();
        let c1 = commit_file(&repo, "f", "1\n", None);
        let index_path = repo.index_path_for_env().expect("index path");
        std::fs::write(&index_path, b"not an index").expect("corrupt index");

        reset_to_commit(
            &repo,
            &reset_opts(c1, ResetMode::Mixed),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("reset");

        let index = repo.load_index().expect("index readable again");
        assert!(index.get(b"f", 0).is_some());
    }

    #[test]
    fn hard_reset_rewrites_racy_unchanged_file_with_same_size_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repo, trimmed) = wide_repo_with_trimmed_branch(root);
        git(root, &["config", "core.trustctime", "false"]);
        pin_worktree_mtimes_and_refresh(root, 1_700_000_000);

        // Edit a file that is identical on both sides of the reset without changing its size
        // or mtime, then make the index as old as that mtime so the entry is racily clean.
        let kept = root.join(format!("d{WIDE_DELETED_DIRS:02}/f00.txt"));
        let original = std::fs::read(&kept).expect("read kept");
        let mut edited = original.clone();
        edited[0] = b'X';
        std::fs::write(&kept, &edited).expect("same-size edit");
        let pinned = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&kept, pinned).expect("forge mtime");
        let index_path = repo.index_path_for_env().expect("index path");
        filetime::set_file_mtime(&index_path, pinned).expect("racy index");
        let index = repo.load_index().expect("index");
        let rel = format!("d{WIDE_DELETED_DIRS:02}/f00.txt");
        let entry = index.get(rel.as_bytes(), 0).expect("entry");
        assert!(
            crate::diff::entry_is_racy(entry, crate::diff::index_mtime_for_diff(&index, None)),
            "fixture entry must be racy"
        );

        reset_to_commit(
            &repo,
            &reset_opts(trimmed, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset");

        assert_eq!(std::fs::read(&kept).expect("read"), original);
        assert_git_sees_clean_repo(root);
    }

    #[test]
    fn hard_reset_keeps_recorded_stat_rows_for_unchanged_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repo, trimmed) = wide_repo_with_trimmed_branch(root);
        git(root, &["config", "core.trustctime", "false"]);
        pin_worktree_mtimes_and_refresh(root, 1_700_000_000);

        // Stand-in for a directory scan whose ctime disagrees with the path stat Git recorded
        // (NTFS read_dir vs lstat): the stat check ignores ctime, so the row stays clean, and
        // the reset must keep the recorded row rather than store what the scan saw.
        let kept = format!("d{WIDE_DELETED_DIRS:02}/f00.txt");
        let mut index = repo.load_index().expect("index");
        let entry = index.get_mut(kept.as_bytes(), 0).expect("kept entry");
        entry.ctime_sec = 1_600_000_000;
        entry.ctime_nsec = 7;
        repo.write_index(&mut index).expect("write index");

        reset_to_commit(
            &repo,
            &reset_opts(trimmed, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset");

        let after = repo.load_index().expect("index after");
        let row = after.get(kept.as_bytes(), 0).expect("kept row");
        assert_eq!((row.ctime_sec, row.ctime_nsec), (1_600_000_000, 7));
        assert_git_sees_clean_repo(root);
    }

    /// Repository on `main` with tracked `victim` sized like an empty directory, and branch
    /// `other` that deletes it. Stat checks are minimal (no filemode, ctime or identity), the
    /// index entry is settled at the pinned mtime of `victim`, which is then replaced by an
    /// empty directory with the same size and mtime, so only the file type tells them apart.
    fn file_replaced_by_same_stat_directory(root: &std::path::Path) -> ObjectId {
        git(root, &["init", "-q", "-b", "main", "."]);
        for (key, value) in [
            ("core.filemode", "false"),
            ("core.checkStat", "minimal"),
            ("core.trustctime", "false"),
        ] {
            git(root, &["config", key, value]);
        }
        let probe = root.join("probe");
        std::fs::create_dir(&probe).expect("probe dir");
        let dir_size = std::fs::metadata(&probe).expect("probe meta").len() as usize;
        std::fs::remove_dir(&probe).expect("remove probe");
        let pinned = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        std::fs::write(root.join("victim"), vec![b'a'; dir_size]).expect("victim");
        std::fs::write(root.join("keep"), "keep\n").expect("keep");
        for name in ["victim", "keep"] {
            filetime::set_file_mtime(root.join(name), pinned).expect("pin");
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["switch", "-qc", "other"]);
        git(root, &["rm", "-q", "victim"]);
        git(root, &["commit", "-qm", "drop victim"]);
        let other = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("oid");
        git(root, &["switch", "-q", "main"]);
        filetime::set_file_mtime(root.join("victim"), pinned).expect("re-pin");
        git(root, &["update-index", "-q", "--refresh"]);
        std::fs::remove_file(root.join("victim")).expect("remove victim");
        std::fs::create_dir(root.join("victim")).expect("victim dir");
        filetime::set_file_mtime(root.join("victim"), pinned).expect("pin dir");
        assert_eq!(
            std::fs::metadata(root.join("victim"))
                .expect("dir meta")
                .len() as usize,
            dir_size
        );
        other
    }

    fn open_at(root: &std::path::Path) -> Repository {
        Repository::open(
            &root.join(".git"),
            &crate::environment::RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open")
    }

    #[test]
    fn hard_reset_restores_file_replaced_by_same_stat_directory_like_git() {
        let grit_dir = tempfile::tempdir().expect("tempdir");
        let git_dir = tempfile::tempdir().expect("tempdir");
        file_replaced_by_same_stat_directory(grit_dir.path());
        file_replaced_by_same_stat_directory(git_dir.path());

        git(git_dir.path(), &["reset", "-q", "--hard", "HEAD"]);
        let repo = open_at(grit_dir.path());
        let head =
            ObjectId::from_hex(git(grit_dir.path(), &["rev-parse", "HEAD"]).trim()).expect("head");
        reset_to_commit(
            &repo,
            &reset_opts(head, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset");

        for root in [git_dir.path(), grit_dir.path()] {
            let victim = root.join("victim");
            assert!(victim.is_file(), "{} restores the file", root.display());
            assert_eq!(
                std::fs::read(&victim).expect("read victim"),
                std::fs::read(git_dir.path().join("victim")).expect("git victim")
            );
            assert_git_sees_clean_repo(root);
        }
    }

    #[test]
    fn switch_refuses_to_delete_file_replaced_by_same_stat_directory_like_git() {
        use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};

        let grit_dir = tempfile::tempdir().expect("tempdir");
        let git_dir = tempfile::tempdir().expect("tempdir");
        let other = file_replaced_by_same_stat_directory(grit_dir.path());
        file_replaced_by_same_stat_directory(git_dir.path());

        let git_switch = std::process::Command::new("git")
            .current_dir(git_dir.path())
            .args(["switch", "-q", "other"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git switch");
        assert!(!git_switch.status.success(), "git refuses the switch");

        let repo = open_at(grit_dir.path());
        switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "other".into(),
                    oid: other,
                },
                dry_run: false,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("grit refuses the switch too");
        assert!(
            grit_dir.path().join("victim").is_dir(),
            "directory untouched"
        );
        assert_eq!(
            git(grit_dir.path(), &["symbolic-ref", "--short", "HEAD"]).trim(),
            "main"
        );
    }

    #[test]
    fn hard_reset_removes_staged_only_paths_and_keeps_untracked_under_deleted_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repo, trimmed) = wide_repo_with_trimmed_branch(root);
        std::fs::write(root.join("d00/staged.txt"), "new\n").expect("staged");
        git(root, &["add", "d00/staged.txt"]);
        std::fs::write(root.join("d01/untracked.txt"), "keep me\n").expect("untracked");

        reset_to_commit(
            &repo,
            &reset_opts(trimmed, ResetMode::Hard),
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("hard reset");

        assert!(
            !root.join("d00").exists(),
            "staged-only path and its dir removed"
        );
        assert_eq!(
            std::fs::read(root.join("d01/untracked.txt")).expect("untracked kept"),
            b"keep me\n"
        );
        let tracked: Vec<_> = std::fs::read_dir(root.join("d01"))
            .expect("d01")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            tracked,
            ["untracked.txt"],
            "only the untracked file remains"
        );
        assert_eq!(git(root, &["status", "--porcelain"]), "?? d01/\n");
        git(root, &["fsck", "--no-progress"]);
    }
}
