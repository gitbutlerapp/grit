//! Create a commit from the current index and move the checked-out branch.
//!
//! [`create_commit`] performs incremental cache-tree write-tree, writes the commit
//! object, persists the index with a valid cache-tree, then updates the branch and `HEAD`
//! reflogs through [`crate::refs::update_branch_for_commit`].

use crate::config::ConfigSet;
use crate::diff::{diff_trees, zero_oid};
use crate::error::{Error, Result};
use crate::hooks::{run_commit_hook_checked, CommitHookEnv};
use crate::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use crate::progress::ProgressSink;
use crate::refs::{update_branch_for_commit_with_config, BranchCommitRefUpdate};
use crate::repo::Repository;
use crate::signing::{
    extra_headers_without_commit_signatures, should_sign_commit, sign_serialized_commit, GpgConfig,
};
use crate::state::{finish_merge_state, read_merge_heads, resolve_head, HeadState};
use crate::write_tree::{is_empty_tree_oid, write_tree_update_index, WriteTreeFlags};
use std::fs;
use std::path::Path;

/// Inputs for [`create_commit`].
#[derive(Debug, Clone)]
pub struct CommitRequest {
    /// Commit message body (first line becomes the subject). A trailing newline is added when missing.
    pub message: String,
    /// Author identity in Git header form: `Name <email> <unix-time> <tz>`.
    pub author: String,
    /// Committer identity in the same form as [`Self::author`].
    pub committer: String,
    /// When false (default), reject commits whose tree equals the parent's tree or is the empty tree on an unborn branch.
    pub allow_empty: bool,
    /// When `Some(true)`, sign even if `commit.gpgsign` is false; when `Some(false)`, skip signing even when config requests it. When `None`, honor `commit.gpgsign`.
    pub sign_override: Option<bool>,
    /// Replace the current branch tip: parents and author come from the tip being replaced; an empty
    /// [`Self::message`] reuses that commit's message. Same-tree amends are allowed.
    pub amend: bool,
}

/// Result of [`create_commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    /// OID of the new commit object written to the object database.
    pub oid: ObjectId,
    /// Short branch name (e.g. `main`, not `refs/heads/main`).
    pub branch: String,
    /// Parent commit OID, or `None` for the root commit on an unborn branch.
    pub parent: Option<ObjectId>,
    /// Number of path-level changes between the parent commit tree and this commit's tree.
    pub changes: usize,
}

/// Write a commit from the current index and advance the checked-out branch.
///
/// Loads the index from disk, builds the commit tree with an incremental cache-tree
/// write-tree, writes the commit object, writes the index back with an updated cache-tree,
/// then atomically updates the branch ref and reflogs (see [`crate::refs::update_branch_for_commit_with_config`]).
///
/// # Parameters
///
/// - `repo` — open repository with a work tree and branch `HEAD`.
/// - `req` — message, author/committer identities, and empty-commit policy.
/// - `progress` — optional progress sink (single `commit` phase).
///
/// # Errors
///
/// - [`Error::DetachedHead`] when `HEAD` is not on a branch.
/// - [`Error::AmendUnborn`] when [`CommitRequest::amend`] is set but the branch has no commits yet.
/// - [`Error::NothingToCommit`] when the new tree is empty (unborn branch) or matches the parent tree and [`CommitRequest::allow_empty`] is false (not applied when amending).
/// - [`Error::IndexUnmerged`] when the index has conflict stages.
/// - Index write or diff failures before the branch is updated; the branch tip is not advanced.
/// - Ref/reflog failures from [`crate::refs::update_branch_for_commit_with_config`]; the branch tip is not left advanced without reflogs when logging is enabled.
pub fn create_commit(
    repo: &Repository,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
) -> Result<CommitOutcome> {
    progress.start("commit", None);

    let head = resolve_head(&repo.git_dir)?;
    let (refname, short_name, mut parent) = match head {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        HeadState::Detached { .. } => return Err(Error::DetachedHead),
        HeadState::Invalid => {
            return Err(Error::Message("HEAD is in an unknown state".into()));
        }
    };

    let mut amend_parents = Vec::new();
    let mut amend_author = None;
    let mut amend_author_raw = Vec::new();
    let mut amend_extra_headers = Vec::new();
    let mut amend_default_message = None;
    let mut replaced_tip = None;
    if req.amend {
        let Some(tip) = parent.take() else {
            return Err(Error::AmendUnborn);
        };
        let obj = repo.odb.read(&tip)?;
        if obj.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "expected commit, got {}",
                obj.kind.as_str()
            )));
        }
        let old = parse_commit(&obj.data)?;
        amend_parents = old.parents.clone();
        parent = old.parents.first().copied();
        amend_author = Some(old.author);
        amend_author_raw = old.author_raw;
        amend_extra_headers = extra_headers_without_commit_signatures(&old.extra_headers);
        amend_default_message = Some(old.message);
        replaced_tip = Some(tip);
    }

    let mut index = repo.load_index()?;
    if index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let merge_heads = read_merge_heads(&repo.git_dir)?;
    let concluding_merge = !merge_heads.is_empty();
    let index_path = repo.git_dir.join("index");
    let commit_env = CommitHookEnv {
        index_file: Some(index_path.as_path()),
        git_editor: Some(":"),
        git_prefix: None,
        extra_env: &[],
    };
    run_commit_hook_checked(repo, "pre-commit", &[], None, &commit_env)?;

    let parent_tree = parent
        .as_ref()
        .map(|parent_oid| commit_tree_oid(repo, parent_oid))
        .transpose()?;

    let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())?;

    if !req.allow_empty && !req.amend {
        if parent_tree.is_none() && is_empty_tree_oid(&repo.odb, &tree) {
            return Err(Error::NothingToCommit);
        }
        if !concluding_merge {
            if let Some(ref old) = parent_tree {
                if old == &tree {
                    return Err(Error::NothingToCommit);
                }
            }
        }
    }

    let mut message = req.message.trim().to_owned();
    if req.amend && message.is_empty() {
        message = amend_default_message.unwrap_or_default();
    }
    if !message.is_empty() {
        message.push('\n');
    }
    let editmsg_path = repo.git_dir.join("COMMIT_EDITMSG");
    fs::write(&editmsg_path, &message).map_err(Error::Io)?;
    let editmsg_arg = commit_editmsg_hook_argument(repo, &editmsg_path);
    run_commit_hook_checked(
        repo,
        "commit-msg",
        &[editmsg_arg.as_str()],
        None,
        &commit_env,
    )?;
    message = fs::read_to_string(&editmsg_path).map_err(Error::Io)?;
    if message.is_empty() {
        return Err(Error::Message("empty commit message".into()));
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }

    let (author, author_raw) = if let Some(a) = amend_author {
        (a, amend_author_raw)
    } else {
        (req.author.clone(), Vec::new())
    };
    let mut commit_parents = if req.amend {
        amend_parents
    } else {
        parent.into_iter().collect()
    };
    if !req.amend {
        for merge_head in merge_heads {
            if !commit_parents.iter().any(|p| p == &merge_head) {
                commit_parents.push(merge_head);
            }
        }
    }
    let commit_data = CommitData {
        tree,
        parents: commit_parents,
        author,
        committer: req.committer.clone(),
        author_raw,
        committer_raw: Vec::new(),
        encoding: None,
        message,
        raw_message: None,
        extra_headers: amend_extra_headers,
    };
    let config = repo.config()?;
    let bytes = prepare_commit_bytes(repo, config.as_ref(), &commit_data, req.sign_override)?;
    let oid = repo.odb.write(ObjectKind::Commit, &bytes)?;

    let previous_tip = replaced_tip.or(parent);
    let reflog_old = previous_tip.unwrap_or_else(zero_oid);
    let subject = commit_subject(&commit_data.message);
    let reflog_msg = if req.amend {
        format!("commit (amend): {subject}")
    } else if parent.is_some() {
        format!("commit: {subject}")
    } else {
        format!("commit (initial): {subject}")
    };

    let expected_old = previous_tip.or_else(|| {
        if crate::refs::resolve_ref(&repo.git_dir, &refname).is_ok() {
            Some(reflog_old)
        } else {
            None
        }
    });

    repo.write_index(&mut index)?;

    let changes = diff_trees(&repo.odb, parent_tree.as_ref(), Some(&tree), "")?.len();

    update_branch_for_commit_with_config(
        &repo.git_dir,
        &BranchCommitRefUpdate {
            branch_ref: &refname,
            expected_old,
            new_oid: oid,
            identity: &req.committer,
            reflog_message: &reflog_msg,
        },
        config.as_ref(),
    )?;

    if concluding_merge {
        let _ = finish_merge_state(repo)?;
    }

    let _ = run_commit_hook_checked(repo, "post-commit", &[], None, &commit_env);

    progress.finish();
    Ok(CommitOutcome {
        oid,
        branch: short_name,
        parent,
        changes,
    })
}

/// Serialize `data` and sign the bytes when [`should_sign_commit`] says so.
///
/// # Errors
///
/// Propagates signing and config errors from [`GpgConfig::from_config`] and
/// [`sign_serialized_commit`].
pub fn prepare_commit_bytes(
    repo: &Repository,
    config: &ConfigSet,
    data: &CommitData,
    sign_override: Option<bool>,
) -> Result<Vec<u8>> {
    let unsigned = serialize_commit(data);
    if !should_sign_commit(config, sign_override)? {
        return Ok(unsigned);
    }
    let gpg = GpgConfig::from_config(config)?.with_command_runner(repo.command_runner());
    sign_serialized_commit(&gpg, &unsigned, &data.committer, None, repo.odb.hash_algo())
}

/// Write a commit object to the ODB, honoring `commit.gpgsign` and `sign_override`.
///
/// # Errors
///
/// Same as [`prepare_commit_bytes`], plus object-database write failures.
pub fn write_commit_object(
    repo: &Repository,
    data: &CommitData,
    sign_override: Option<bool>,
) -> Result<ObjectId> {
    let config = repo.config()?;
    let bytes = prepare_commit_bytes(repo, config.as_ref(), data, sign_override)?;
    repo.odb.write(ObjectKind::Commit, &bytes)
}

fn commit_tree_oid(repo: &Repository, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "expected commit, got {}",
            obj.kind.as_str()
        )));
    }
    Ok(parse_commit(&obj.data)?.tree)
}

fn commit_subject(message: &str) -> &str {
    message.split('\n').next().unwrap_or("").trim_end()
}

/// Path passed as `argv[1]` to the `commit-msg` hook (Git: `.git/COMMIT_EDITMSG` when possible).
fn commit_editmsg_hook_argument(repo: &Repository, editmsg_path: &Path) -> String {
    if let Some(wt) = repo.work_tree.as_deref() {
        if let Ok(rel) = editmsg_path.strip_prefix(wt) {
            let s = rel.to_string_lossy();
            if !s.is_empty() {
                return s.replace('\\', "/");
            }
        }
    }
    editmsg_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::index::{entry_from_stat, Index, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::porcelain::add::{stage, StageOptions};
    use crate::progress::NullProgress;
    use crate::reflog::read_reflog;
    use crate::refs::{resolve_ref, set_test_inject_reflog_fail};
    use crate::repo::set_test_inject_index_write_fail;
    use crate::write_tree::{cache_tree_fully_valid, verify_cache_tree};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn init_repo(root: &Path) -> Repository {
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n",
        )
        .unwrap();
        Repository::open(&git, Some(root)).unwrap()
    }

    fn ident() -> String {
        "Test User <t@example.com> 1 +0000".to_owned()
    }

    fn stage_file_in_index(repo: &Repository, index: &mut Index, rel: &str, contents: &[u8]) {
        let wt = repo.work_tree.as_ref().unwrap();
        let abs = wt.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, contents).unwrap();
        let oid = repo.odb.write(ObjectKind::Blob, contents).unwrap();
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
    }

    fn write_index(repo: &Repository, index: &mut Index) {
        index.sort();
        repo.write_index(index).unwrap();
    }

    fn commit_req(message: &str) -> CommitRequest {
        let ident = ident();
        CommitRequest {
            message: message.to_owned(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
        }
    }

    #[test]
    fn create_commit_initial() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("README"), b"hello\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let outcome = create_commit(&repo, &commit_req("initial"), &mut NullProgress).unwrap();
        assert!(outcome.parent.is_none());
        assert_eq!(outcome.branch, "main");
        assert_eq!(outcome.changes, 1);
        assert_eq!(
            resolve_ref(&repo.git_dir, "refs/heads/main").unwrap(),
            outcome.oid
        );
    }

    #[test]
    fn create_commit_rejects_empty_initial_index() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let err = create_commit(&repo, &commit_req("empty"), &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
        assert!(resolve_ref(&repo.git_dir, "refs/heads/main").is_err());
    }

    #[test]
    fn create_commit_normal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let outcome = create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();
        assert_eq!(outcome.changes, 1);
        assert!(outcome.parent.is_some());
    }

    #[test]
    fn create_commit_nothing_to_commit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        let err = create_commit(&repo, &commit_req("noop"), &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
    }

    #[test]
    fn create_commit_detached_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let first = create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
        fs::write(repo.git_dir.join("HEAD"), first.oid.to_hex()).unwrap();

        let err = create_commit(&repo, &commit_req("detached"), &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::DetachedHead));
    }

    #[test]
    fn create_commit_updates_reflogs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let outcome = create_commit(&repo, &commit_req("subject line"), &mut NullProgress).unwrap();

        let branch_log = read_reflog(&repo.git_dir, "refs/heads/main").unwrap();
        assert_eq!(branch_log.len(), 1);
        assert_eq!(branch_log[0].new_oid, outcome.oid);
        assert_eq!(branch_log[0].message, "commit (initial): subject line");

        let head_log = read_reflog(&repo.git_dir, "HEAD").unwrap();
        assert_eq!(head_log.len(), 1);
        assert_eq!(head_log[0].new_oid, outcome.oid);
    }

    #[test]
    fn create_commit_index_write_failure_does_not_advance_branch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        set_test_inject_index_write_fail(true);
        let err = create_commit(&repo, &commit_req("fail"), &mut NullProgress).unwrap_err();
        set_test_inject_index_write_fail(false);
        assert!(!matches!(err, Error::NothingToCommit));

        assert!(
            resolve_ref(&repo.git_dir, "refs/heads/main").is_err(),
            "branch must not advance when index write fails"
        );
    }

    #[test]
    fn create_commit_rolls_back_branch_when_head_reflog_fails() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        set_test_inject_reflog_fail(Some("HEAD"));
        let err = create_commit(&repo, &commit_req("fail"), &mut NullProgress).unwrap_err();
        set_test_inject_reflog_fail(None);
        assert!(!matches!(err, Error::NothingToCommit));

        assert!(
            resolve_ref(&repo.git_dir, "refs/heads/main").is_err(),
            "branch must not advance when HEAD reflog fails"
        );
        assert!(read_reflog(&repo.git_dir, "refs/heads/main")
            .unwrap()
            .is_empty());
        assert!(read_reflog(&repo.git_dir, "HEAD").unwrap().is_empty());
    }

    #[test]
    fn create_commit_leaves_valid_cache_tree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "dir/one.txt", b"1\n");
        stage_file_in_index(&repo, &mut index, "dir/two.txt", b"2\n");
        write_index(&repo, &mut index);

        let outcome = create_commit(&repo, &commit_req("tree"), &mut NullProgress).unwrap();

        let index = repo.load_index().unwrap();
        verify_cache_tree(&index).expect("cache-tree valid");
        assert!(cache_tree_fully_valid(&repo.odb, index.cache_tree.as_ref()));
        let commit_obj = repo.odb.read(&outcome.oid).unwrap();
        let tree = parse_commit(&commit_obj.data).unwrap().tree;
        assert_eq!(index.cache_tree_root, Some(tree));
    }

    #[test]
    fn create_commit_amend_same_tree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();

        let mut req = commit_req("");
        req.amend = true;
        let tip = resolve_ref(&repo.git_dir, "refs/heads/main").unwrap();
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
        assert_ne!(outcome.oid, tip);
        assert!(outcome.parent.is_some());

        let branch_log = read_reflog(&repo.git_dir, "refs/heads/main").unwrap();
        assert_eq!(branch_log.len(), 3);
        assert_eq!(branch_log[2].message, "commit (amend): second");
    }

    #[test]
    fn create_commit_amend_unborn() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        fs::write(tmp.path().join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("nope");
        req.amend = true;
        let err = create_commit(&repo, &req, &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::AmendUnborn));
    }

    #[test]
    fn create_commit_allow_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        let mut req = commit_req("empty");
        req.allow_empty = true;
        let parent = resolve_ref(&repo.git_dir, "refs/heads/main").unwrap();
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
        assert_eq!(outcome.changes, 0);
        assert_eq!(outcome.parent, Some(parent));
        assert_ne!(outcome.oid, parent);
    }
}
