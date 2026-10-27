//! Index-and-worktree commands take `index.lock` before touching anything.
//!
//! With a foreign `.git/index.lock` in place, stash push/apply, hard and keep reset, switch and
//! restore must fail with [`Error::LockHeld`] and leave the work tree, index, `HEAD`, refs and
//! object database exactly as they were. Once the lock is gone the same operations succeed and
//! system `git` reads the result.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::stash::{StashApplyOptions, StashMode, StashPushOptions};
use grit_lib::PostOperationHooks;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "1000000000 +0000")
        .env("GIT_COMMITTER_DATE", "1000000000 +0000")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `main` has `a.txt = base`; branch `other` changes it to `other` and adds `o.txt`.
fn fixture() -> (TempDir, Repository) {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    fs::write(root.join("a.txt"), "base\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["checkout", "-q", "-b", "other"]);
    fs::write(root.join("a.txt"), "other\n").expect("write");
    fs::write(root.join("o.txt"), "only on other\n").expect("write");
    git(root, &["add", "a.txt", "o.txt"]);
    git(root, &["commit", "-q", "-m", "other"]);
    git(root, &["checkout", "-q", "main"]);
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo");
    (tmp, repo)
}

fn identity() -> grit_lib::ident::Signature {
    "Test <t@example.com> 1000000000 +0000"
        .parse()
        .expect("signature")
}

fn rev(root: &Path, spec: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", spec]).trim()).expect("oid")
}

fn collect_files(dir: &Path, base: &Path, skip_git: bool, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if skip_git && entry.file_name() == ".git" {
            continue;
        }
        let file_type = entry.file_type().expect("file type");
        let rel = path.strip_prefix(base).expect("relative").to_path_buf();
        if file_type.is_dir() {
            collect_files(&path, base, skip_git, out);
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path).expect("read link");
            out.insert(rel, target.to_string_lossy().into_owned().into_bytes());
        } else {
            out.insert(rel, fs::read(&path).expect("read file"));
        }
    }
}

/// Every byte a failed command could have changed: work tree, index, refs, logs, objects.
#[derive(Debug, PartialEq, Eq)]
struct RepoState {
    worktree: BTreeMap<PathBuf, Vec<u8>>,
    git_dir: BTreeMap<PathBuf, Vec<u8>>,
}

fn repo_state(root: &Path) -> RepoState {
    let mut worktree = BTreeMap::new();
    collect_files(root, root, true, &mut worktree);
    let mut git_dir = BTreeMap::new();
    let dot_git = root.join(".git");
    collect_files(&dot_git, &dot_git, false, &mut git_dir);
    RepoState { worktree, git_dir }
}

/// Run `op` while a foreign `index.lock` exists and assert it changed nothing.
fn assert_refused_by_foreign_lock<T: std::fmt::Debug>(
    root: &Path,
    op: impl FnOnce() -> grit_lib::error::Result<T>,
) {
    let lock_path = root.join(".git/index.lock");
    fs::write(&lock_path, b"foreign\n").expect("foreign lock");
    let before = repo_state(root);

    let err = op().expect_err("operation must fail while index.lock is held");
    match &err {
        Error::LockHeld { path, .. } => assert_eq!(path, &lock_path),
        other => panic!("expected Error::LockHeld, got {other:?}"),
    }

    let after = repo_state(root);
    assert_eq!(
        after.worktree, before.worktree,
        "work tree must be unchanged"
    );
    assert_eq!(
        after.git_dir, before.git_dir,
        "index, refs, reflogs and objects must be unchanged"
    );
    assert_eq!(
        fs::read(&lock_path).expect("lock still there"),
        b"foreign\n",
        "someone else's lock must be left alone"
    );
    fs::remove_file(&lock_path).expect("remove foreign lock");
    git(root, &["fsck", "--strict", "--no-progress"]);
}

fn stash_opts(mode: StashMode, include_untracked: bool, pathspecs: &[&str]) -> StashPushOptions {
    StashPushOptions {
        message: None,
        include_untracked,
        identity: identity(),
        selection: None,
        pathspecs: pathspecs.iter().map(|s| (*s).to_owned()).collect(),
        dry_run: false,
        mode,
    }
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).expect("read")
}

#[test]
fn stash_push_with_held_index_lock_keeps_edits() {
    let (tmp, repo) = fixture();
    let root = tmp.path();
    fs::write(root.join("a.txt"), "precious\n").expect("write");
    fs::write(root.join("b.txt"), "newfile\n").expect("write");

    assert_refused_by_foreign_lock(root, || {
        repo.stash().push(&stash_opts(StashMode::All, false, &[]))
    });

    let pushed = repo
        .stash()
        .push(&stash_opts(StashMode::All, false, &[]))
        .expect("push")
        .expect("something to stash");
    assert!(!root.join(".git/index.lock").exists());
    assert_eq!(read(root, "a.txt"), "base\n");
    assert_eq!(read(root, "b.txt"), "newfile\n");
    assert_eq!(rev(root, "refs/stash"), pushed.oid);
    assert_eq!(git(root, &["status", "--porcelain"]), "?? b.txt\n");
    git(root, &["fsck", "--strict", "--no-progress"]);
}

#[test]
fn stash_push_untracked_with_held_index_lock_keeps_untracked_files() {
    let (tmp, repo) = fixture();
    let root = tmp.path();
    fs::write(root.join("a.txt"), "precious\n").expect("write");
    fs::create_dir_all(root.join("dir")).expect("mkdir");
    fs::write(root.join("dir/d.txt"), "untracked\n").expect("write");

    assert_refused_by_foreign_lock(root, || {
        repo.stash().push(&stash_opts(StashMode::All, true, &[]))
    });

    repo.stash()
        .push(&stash_opts(StashMode::All, true, &[]))
        .expect("push")
        .expect("something to stash");
    assert!(!root.join("dir").exists());
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    git(root, &["stash", "pop", "-q"]);
    assert_eq!(read(root, "dir/d.txt"), "untracked\n");
    assert_eq!(read(root, "a.txt"), "precious\n");
}

#[test]
fn stash_push_modes_with_held_index_lock_change_nothing() {
    let cases: [(StashMode, &[&str]); 4] = [
        (StashMode::KeepIndex, &[]),
        (StashMode::StagedOnly, &[]),
        (StashMode::All, &["a.txt"]),
        (StashMode::StagedOnly, &["s.txt"]),
    ];
    for (mode, pathspecs) in cases {
        let (tmp, repo) = fixture();
        let root = tmp.path();
        fs::write(root.join("s.txt"), "staged\n").expect("write");
        git(root, &["add", "s.txt"]);
        fs::write(root.join("a.txt"), "precious\n").expect("write");

        assert_refused_by_foreign_lock(root, || {
            repo.stash().push(&stash_opts(mode, false, pathspecs))
        });

        repo.stash()
            .push(&stash_opts(mode, false, pathspecs))
            .expect("push")
            .expect("something to stash");
        assert!(!root.join(".git/index.lock").exists());
        git(root, &["fsck", "--strict", "--no-progress"]);
        assert_eq!(git(root, &["stash", "list"]).lines().count(), 1);
    }
}

#[test]
fn stash_apply_with_held_index_lock_changes_nothing() {
    let (tmp, repo) = fixture();
    let root = tmp.path();
    fs::write(root.join("a.txt"), "precious\n").expect("write");
    git(root, &["stash", "-q"]);

    assert_refused_by_foreign_lock(root, || {
        repo.stash().apply(0, &StashApplyOptions::default())
    });

    repo.stash()
        .apply(0, &StashApplyOptions::default())
        .expect("apply");
    assert_eq!(read(root, "a.txt"), "precious\n");
    assert_eq!(git(root, &["status", "--porcelain"]), " M a.txt\n");
}

#[test]
fn hard_reset_with_held_index_lock_changes_nothing() {
    let (tmp, repo) = fixture();
    let root = tmp.path();
    fs::write(root.join("n.txt"), "new\n").expect("write");
    git(root, &["add", "n.txt"]);
    fs::write(root.join("a.txt"), "precious\n").expect("write");
    let head = rev(root, "HEAD");

    let opts = ResetOptions::new(head, ResetMode::Hard, identity(), "reset: moving to HEAD");
    assert_refused_by_foreign_lock(root, || {
        reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel)
    });
    assert_eq!(
        git(root, &["status", "--porcelain"]),
        " M a.txt\nA  n.txt\n"
    );

    reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel).expect("reset");
    assert!(!root.join("n.txt").exists());
    assert_eq!(read(root, "a.txt"), "base\n");
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn hard_reset_to_other_commit_with_held_index_lock_changes_nothing() {
    let (tmp, repo) = fixture();
    let root = tmp.path();
    let target = rev(root, "other");
    let opts = ResetOptions::new(
        target,
        ResetMode::Hard,
        identity(),
        "reset: moving to other",
    );

    assert_refused_by_foreign_lock(root, || {
        reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel)
    });

    reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel).expect("reset");
    assert_eq!(rev(root, "HEAD"), target);
    assert_eq!(read(root, "a.txt"), "other\n");
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn keep_reset_with_held_index_lock_changes_nothing() {
    let (tmp, repo) = fixture();
    let root = tmp.path();
    let target = rev(root, "other");
    let opts = ResetOptions::new(
        target,
        ResetMode::Keep,
        identity(),
        "reset: moving to other",
    );

    assert_refused_by_foreign_lock(root, || {
        reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel)
    });

    reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel).expect("reset");
    assert_eq!(read(root, "o.txt"), "only on other\n");
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn switch_with_held_index_lock_changes_nothing() {
    for dirty in [false, true] {
        let (tmp, repo) = fixture();
        let root = tmp.path();
        if dirty {
            fs::write(root.join("u.txt"), "untracked\n").expect("write");
        }
        let opts = SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "other".into(),
                oid: rev(root, "other"),
            },
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        };

        assert_refused_by_foreign_lock(root, || {
            switch_repository(&repo, &opts, &mut NullProgress, &NeverCancel)
        });
        assert_eq!(read(root, "a.txt"), "base\n");
        assert_eq!(
            git(root, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/main"
        );

        switch_repository(&repo, &opts, &mut NullProgress, &NeverCancel).expect("switch");
        assert_eq!(read(root, "a.txt"), "other\n");
        assert_eq!(
            git(root, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/other"
        );
        let expected = if dirty { "?? u.txt\n" } else { "" };
        assert_eq!(git(root, &["status", "--porcelain"]), expected);
    }
}

#[test]
fn restore_with_held_index_lock_changes_nothing() {
    for staged in [false, true] {
        let (tmp, repo) = fixture();
        let root = tmp.path();
        fs::write(root.join("a.txt"), "precious\n").expect("write");
        if staged {
            git(root, &["add", "a.txt"]);
        }
        let head_tree = rev(root, "HEAD^{tree}");
        let opts = RestoreOptions {
            pathspecs: vec!["a.txt".into()],
            pathspec_sources: vec!["a.txt".into()],
            source: if staged {
                RestoreSource::Tree(head_tree)
            } else {
                RestoreSource::Index
            },
            staged,
            worktree: true,
            stage: None,
            remerge: None,
        };

        assert_refused_by_foreign_lock(root, || restore_paths(&repo, &opts));

        restore_paths(&repo, &opts).expect("restore");
        assert_eq!(read(root, "a.txt"), "base\n");
        assert_eq!(git(root, &["status", "--porcelain"]), "");
    }
}
