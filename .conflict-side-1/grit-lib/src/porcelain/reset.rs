//! Move `HEAD` and optionally the index and working tree to a target commit.

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::index::Index;
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use crate::porcelain::merge::tree_to_index_entries;
use crate::refs::{self, append_reflog};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

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
}

impl ResetMode {
    /// Lowercase mode name (`soft`, `mixed`, `hard`) for logging and `--json` output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Mixed => "mixed",
            Self::Hard => "hard",
        }
    }
}

/// Inputs for [`reset_to_commit`].
#[derive(Debug, Clone)]
pub struct ResetOptions {
    /// Commit to reset to.
    pub target: ObjectId,
    /// Soft, mixed (default), or hard reset.
    pub mode: ResetMode,
    /// Reflog identity: `Name <email> <unix-time> <tz>`.
    pub identity: String,
    /// Reflog message (e.g. `reset: moving to HEAD~1`).
    pub message: String,
}

/// Result of [`reset_to_commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetOutcome {
    /// `soft`, `mixed`, or `hard`.
    pub mode: ResetMode,
    /// Commit `HEAD` points at after the reset.
    pub target: ObjectId,
    /// Previous `HEAD` commit before the reset.
    pub previous: ObjectId,
    /// Branch ref updated, or `HEAD` when detached.
    pub ref_updated: String,
}

/// Move `HEAD` to `opts.target` according to `opts.mode`.
///
/// Index and worktree updates run before the ref moves so a failure leaves refs unchanged.
///
/// # Errors
///
/// Returns an error when there is no current commit, the work tree is required but
/// missing (hard reset), or ref/index/checkout I/O fails.
pub fn reset_to_commit(repo: &Repository, opts: &ResetOptions) -> Result<ResetOutcome> {
    let head = resolve_head(repo.git_dir())?;
    let previous = head
        .oid()
        .copied()
        .ok_or_else(|| Error::Message("no commits yet to reset".into()))?;

    let target_tree = commit_tree_oid(repo, &opts.target)?;

    if opts.mode == ResetMode::Hard && repo.work_tree().is_none() {
        return Err(Error::PathError(
            "cannot hard reset a bare repository's working tree".into(),
        ));
    }

    match opts.mode {
        ResetMode::Soft => {}
        ResetMode::Mixed => {
            let mut index = index_from_tree(repo, &target_tree)?;
            repo.write_index(&mut index)?;
        }
        ResetMode::Hard => {
            let head_tree = commit_tree_oid(repo, &previous)?;
            checkout_hard_to_tree_from_head_tree(repo, &head_tree, &target_tree)?;
        }
    }

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
            HeadState::Invalid => return Err(Error::Message("HEAD is in an unknown state".into())),
        }
    } else {
        ref_label_for_head(&head)
    };

    Ok(ResetOutcome {
        mode: opts.mode,
        target: opts.target,
        previous,
        ref_updated,
    })
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
    Ok(parse_commit(&obj.data)?.tree)
}

fn index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Index> {
    let entries = tree_to_index_entries(repo, tree_oid, "")?;
    let mut index = Index::new();
    for entry in entries {
        index.push_entry_unsorted(entry);
    }
    index.sort();
    Ok(index)
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

    fn commit_file(
        repo: &Repository,
        path: &str,
        contents: &str,
        parent: Option<ObjectId>,
    ) -> ObjectId {
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join(path), contents).expect("write");
        let mut index = repo.load_index().expect("index");
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

    fn reset_opts(target: ObjectId, mode: ResetMode) -> ResetOptions {
        ResetOptions {
            target,
            mode,
            identity: "T <t@t> 0 +0000".into(),
            message: "reset".into(),
        }
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
}
