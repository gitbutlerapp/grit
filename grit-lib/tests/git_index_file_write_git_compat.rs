//! `GIT_INDEX_FILE` must select the index for reads and writes (issue #2353).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope, HookPolicy};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open_with_index(worktree: &Path, index_file: &Path) -> Repository {
    let mut environment = Environment::empty();
    environment.cwd = worktree.to_owned();
    environment.git_index_file = Some(
        index_file
            .canonicalize()
            .unwrap_or_else(|_| index_file.to_path_buf())
            .to_string_lossy()
            .into_owned(),
    );
    Repository::open(
        &worktree.join(".git"),
        &RepositoryOptions::with_environment(environment).with_work_tree(worktree),
    )
    .expect("open repository")
}

fn git_ls_files(worktree: &Path, index_file: Option<&Path>) -> Vec<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree).args(["ls-files"]);
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if let Some(index) = index_file {
        cmd.env(
            "GIT_INDEX_FILE",
            index.canonicalize().unwrap_or_else(|_| index.to_path_buf()),
        );
    }
    let out = cmd.output().expect("git ls-files");
    assert!(
        out.status.success(),
        "git ls-files: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

fn init_repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "a@b"]);
    git(&root, &["config", "user.name", "n"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("a"), b"1\n").expect("write a");
    fs::write(root.join("b"), b"2\n").expect("write b");
    git(&root, &["add", "a", "b"]);
    git(&root, &["commit", "-qm", "init"]);
    (tmp, root)
}

fn commit_req(message: &str) -> CommitRequest {
    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    let mut req = CommitRequest::new(message);
    req.author = Some(ident.parse().expect("signature"));
    req.committer = Some(ident.parse().expect("signature"));
    req.hooks = HookPolicy::SkipVerify;
    req
}

#[test]
fn commit_staged_with_git_index_file_leaves_default_index_unchanged() {
    let (_tmp, root) = init_repo();
    fs::write(root.join("c"), b"c\n").expect("write c");
    git(&root, &["add", "c"]);
    let alt_index = root.join("alt-index");
    fs::copy(root.join(".git/index"), &alt_index).expect("copy index");
    git(&root, &["reset", "-q"]);

    let default_before = fs::read(root.join(".git/index")).expect("read default index");
    assert_eq!(git_ls_files(&root, None), vec!["a", "b"]);
    assert_eq!(git_ls_files(&root, Some(&alt_index)), vec!["a", "b", "c"]);

    let repo = open_with_index(&root, &alt_index);
    commit_with_scope(
        &repo,
        &commit_req("from temp index"),
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit staged");

    assert_eq!(
        fs::read(root.join(".git/index")).expect("read default index"),
        default_before,
        "default .git/index must be unchanged"
    );
    assert_eq!(git_ls_files(&root, None), vec!["a", "b"]);
    assert_eq!(git_ls_files(&root, Some(&alt_index)), vec!["a", "b", "c"]);
    git(&root, &["fsck"]);
}

#[test]
fn commit_auto_stage_with_git_index_file_leaves_default_index_unchanged() {
    let (_tmp, root) = init_repo();
    fs::write(root.join("d"), b"d\n").expect("write d");
    let alt_index = root.join("alt-index-auto");
    fs::copy(root.join(".git/index"), &alt_index).expect("copy index");

    let default_before = fs::read(root.join(".git/index")).expect("read default index");
    let repo = open_with_index(&root, &alt_index);
    commit_with_scope(
        &repo,
        &commit_req("auto stage temp"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit auto stage");

    assert_eq!(
        fs::read(root.join(".git/index")).expect("read default index"),
        default_before,
        "default .git/index must be unchanged during auto-stage commit"
    );
    git(&root, &["fsck"]);
}

#[test]
fn reset_mixed_with_git_index_file_rewrites_temp_index_only() {
    let (_tmp, root) = init_repo();
    fs::write(root.join("n"), b"n\n").expect("write n");
    git(&root, &["add", "n"]);
    let alt_index = root.join("alt-reset");
    fs::copy(root.join(".git/index"), &alt_index).expect("copy index");
    git(&root, &["reset", "-q", "n"]);

    let default_before = fs::read(root.join(".git/index")).expect("read default index");
    assert!(git_ls_files(&root, Some(&alt_index)).contains(&"n".to_owned()));

    let repo = open_with_index(&root, &alt_index);
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").expect("head");
    reset_to_commit(
        &repo,
        &ResetOptions::new(
            head,
            ResetMode::Mixed,
            "T <t@example.com> 0 +0000".parse().expect("sig"),
            "reset",
        ),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("reset mixed");

    assert_eq!(
        fs::read(root.join(".git/index")).expect("read default index"),
        default_before
    );
    assert!(
        !git_ls_files(&root, Some(&alt_index)).contains(&"n".to_owned()),
        "temp index must be updated by mixed reset"
    );
}

#[test]
fn remove_cached_with_git_index_file_edits_temp_index_only() {
    let (_tmp, root) = init_repo();
    let alt_index = root.join("alt-rm");
    fs::copy(root.join(".git/index"), &alt_index).expect("copy index");

    let default_before = fs::read(root.join(".git/index")).expect("read default index");
    let repo = open_with_index(&root, &alt_index);
    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["a".into()],
            cached: true,
            ..Default::default()
        },
    )
    .expect("rm cached");

    assert_eq!(
        fs::read(root.join(".git/index")).expect("read default index"),
        default_before
    );
    assert_eq!(git_ls_files(&root, None), vec!["a", "b"]);
    assert_eq!(git_ls_files(&root, Some(&alt_index)), vec!["b"]);
}

#[test]
fn pre_commit_hook_sees_git_index_file_not_default_index() {
    let (_tmp, root) = init_repo();
    fs::write(root.join("c"), b"c\n").expect("write c");
    git(&root, &["add", "c"]);
    let alt_index = root.join("alt-hook");
    fs::copy(root.join(".git/index"), &alt_index).expect("copy index");
    git(&root, &["reset", "-q"]);

    let hook = root.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s' \"$GIT_INDEX_FILE\" > \"hook-index-path.txt\"\nexit 0\n",
    )
    .expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    }

    let repo = open_with_index(&root, &alt_index);
    let mut hook_req = CommitRequest::new("hook index");
    hook_req.author = Some("T <t@example.com> 1700000000 +0000".parse().expect("sig"));
    hook_req.committer = Some("T <t@example.com> 1700000000 +0000".parse().expect("sig"));
    hook_req.hooks = HookPolicy::Run;
    commit_with_scope(
        &repo,
        &hook_req,
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit with hook");

    let seen = fs::read_to_string(root.join("hook-index-path.txt")).expect("hook output");
    let alt_canon = alt_index
        .canonicalize()
        .unwrap_or_else(|_| alt_index.clone());
    let default_index = root.join(".git/index");
    assert_ne!(
        PathBuf::from(seen.trim()),
        default_index.canonicalize().unwrap_or(default_index),
        "pre-commit must not receive the default .git/index as GIT_INDEX_FILE"
    );
    assert_eq!(PathBuf::from(seen.trim()), alt_canon);
}
