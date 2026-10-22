//! Path-scoped commits must not mutate `.git/index` when hooks reject the commit.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;

fn ident() -> String {
    "Hook Index <hook@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    }
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Hook Index")
        .env("GIT_AUTHOR_EMAIL", "hook@example.com")
        .env("GIT_COMMITTER_NAME", "Hook Index")
        .env("GIT_COMMITTER_EMAIL", "hook@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn index_bytes(root: &Path) -> Vec<u8> {
    fs::read(root.join(".git/index")).expect("read index")
}

fn cached_names(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--cached", "--name-only"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff --cached");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn install_hook(root: &Path, name: &str, body: &str) {
    let hooks = root.join(".git/hooks");
    fs::create_dir_all(&hooks).expect("hooks dir");
    let path = hooks.join(name);
    fs::write(&path, body).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    }
}

fn seed_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("only.txt"), b"1\n").expect("only");
    fs::write(root.join("unrelated.txt"), b"1\n").expect("unrelated");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);
}

fn paths_only() -> ResolvedPathspecs {
    ResolvedPathspecs {
        specs: vec!["only.txt".into()],
        sources: vec!["only.txt".into()],
        scoped: true,
    }
}

#[test]
fn paths_commit_pre_commit_failure_preserves_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    seed_repo(root);
    fs::write(root.join("only.txt"), b"2\n").expect("edit only");
    fs::write(root.join("unrelated.txt"), b"2\n").expect("edit unrelated");
    git(root, &["add", "unrelated.txt"]);

    install_hook(root, "pre-commit", "#!/bin/sh\nexit 1\n");

    let before_index = index_bytes(root);
    let before_cached = cached_names(root);

    let repo = open_repo(root);
    let err = commit_with_scope(
        &repo,
        &commit_req("partial"),
        CommitScope::Paths(paths_only()),
        &mut NullProgress,
    )
    .expect_err("pre-commit must fail");
    assert!(
        format!("{err}").contains("pre-commit") || format!("{err}").contains("hook"),
        "unexpected error: {err}"
    );

    assert_eq!(index_bytes(root), before_index);
    assert_eq!(cached_names(root), before_cached);
}

#[test]
fn paths_commit_commit_msg_failure_preserves_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    seed_repo(root);
    fs::write(root.join("only.txt"), b"2\n").expect("edit only");
    fs::write(root.join("unrelated.txt"), b"2\n").expect("edit unrelated");
    git(root, &["add", "unrelated.txt"]);

    install_hook(root, "commit-msg", "#!/bin/sh\nexit 1\n");

    let before_index = index_bytes(root);
    let before_cached = cached_names(root);

    let repo = open_repo(root);
    let err = commit_with_scope(
        &repo,
        &commit_req("partial"),
        CommitScope::Paths(paths_only()),
        &mut NullProgress,
    )
    .expect_err("commit-msg must fail");
    assert!(
        format!("{err}").contains("commit-msg") || format!("{err}").contains("hook"),
        "unexpected error: {err}"
    );

    assert_eq!(index_bytes(root), before_index);
    assert_eq!(cached_names(root), before_cached);
}
