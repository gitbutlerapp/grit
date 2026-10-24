//! Grit commit on detached HEAD matches system `git` (issue #1513).

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

fn ident() -> String {
    "Detached Compat <detached@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: Some(id.parse().expect("signature")),
        committer: Some(id.parse().expect("signature")),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        hooks: HookPolicy::SkipVerify,
        ..Default::default()
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Detached Compat")
        .env("GIT_AUTHOR_EMAIL", "detached@example.com")
        .env("GIT_COMMITTER_NAME", "Detached Compat")
        .env("GIT_COMMITTER_EMAIL", "detached@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

#[test]
fn grit_commit_on_detached_head_matches_git() {
    let root = tempfile::tempdir().expect("tmpdir");
    git(root.path(), &["init", "-q", "-b", "main", "."]);
    std::fs::write(root.path().join("a.txt"), b"a\n").unwrap();
    git(root.path(), &["add", "a.txt"]);
    git(root.path(), &["commit", "-m", "one"]);
    let main_after_first = git(root.path(), &["rev-parse", "main"]);

    git(root.path(), &["checkout", "--detach", "HEAD"]);
    std::fs::write(root.path().join("d.txt"), b"d\n").unwrap();

    let repo = open_grit(root.path());
    let outcome = commit_worktree(&repo, &commit_req("detached"), &mut NullProgress).unwrap();
    assert_eq!(outcome.branch, None);

    let grit_head = outcome.oid.to_hex();
    let git_head = git(root.path(), &["rev-parse", "HEAD"]);
    assert_eq!(grit_head, git_head);

    git(root.path(), &["fsck", "--strict"]);

    assert_eq!(
        git(root.path(), &["rev-parse", "main"]),
        main_after_first,
        "branch tip must remain the first commit"
    );
}
