//! Regression for issue #1232: linked worktree remove with inherited system `core.autocrlf`.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::{
    add_linked_worktree, remove_linked_worktree, AddLinkedWorktreeOptions, WorktreeBranch,
};
use grit_test_support::git_cmd as git_shell;

fn write_system_autocrlf_config(dir: &Path) -> PathBuf {
    let path = dir.join("system.gitconfig");
    std::fs::write(&path, "[core]\n\tautocrlf = true\n").expect("system config");
    path
}

fn git_with_system_config(repo: &Path, system_cfg: &Path, args: &[&str]) -> String {
    git_shell(args)
        .in_dir(repo)
        .env("GIT_CONFIG_SYSTEM", system_cfg)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .suc()
        .stdout
}

fn open_grit_inherited_autocrlf(root: &Path, system_cfg: &Path) -> Repository {
    let mut env = Environment::empty();
    env.git_config_system = Some(system_cfg.display().to_string());
    env.git_config_global = Some("/dev/null".into());
    Repository::discover(
        Some(root),
        &RepositoryOptions::with_environment(env).without_format_validation(),
    )
    .expect("open grit repo")
}

fn commit_request(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.to_owned(),
        author: "Test <t@example.com> 1000000000 +0000".into(),
        committer: "Test <t@example.com> 1000000000 +0000".into(),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    }
}

fn git_diff_files_quiet(worktree: &Path, system_cfg: &Path) -> bool {
    Command::new("git")
        .args(["diff-files", "--quiet"])
        .current_dir(worktree)
        .env("GIT_CONFIG_SYSTEM", system_cfg)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git diff-files")
        .success()
}

#[test]
fn remove_linked_worktree_with_system_inherited_autocrlf_and_crlf_worktree_file() {
    let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
    let system_cfg = write_system_autocrlf_config(cfg_dir.path());

    let root = tempfile::tempdir().expect("tempdir");
    let main = root.path().join("main");
    std::fs::create_dir_all(&main).expect("mkdir main");
    git_with_system_config(&main, &system_cfg, &["init", "-q", "-b", "main"]);
    git_with_system_config(&main, &system_cfg, &["config", "user.email", "t@e.t"]);
    git_with_system_config(&main, &system_cfg, &["config", "user.name", "T"]);
    std::fs::write(main.join("a.txt"), b"a\n").expect("seed file");
    git_with_system_config(&main, &system_cfg, &["add", "a.txt"]);
    git_with_system_config(&main, &system_cfg, &["commit", "-qm", "one"]);

    let main_repo = open_grit_inherited_autocrlf(&main, &system_cfg);
    let linked = root.path().join("w");
    add_linked_worktree(
        &main_repo,
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some(WorktreeBranch::Create("f".to_owned())),
            start_point: Some("HEAD".to_owned()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("worktree add");

    std::fs::write(linked.join("new.txt"), b"new\r\n").expect("CRLF new file");
    let linked_repo = open_grit_inherited_autocrlf(&linked, &system_cfg);
    stage(&linked_repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(&linked_repo, &commit_request("two"), &mut NullProgress).expect("commit");

    let porcelain = git_with_system_config(&linked, &system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git status must be clean with inherited system autocrlf, got:\n{porcelain}"
    );
    assert!(
        git_diff_files_quiet(&linked, &system_cfg),
        "git diff-files must be quiet before remove"
    );

    remove_linked_worktree(&main_repo, &linked, false).expect("remove clean linked worktree");
    assert!(
        !linked.exists(),
        "linked worktree directory should be removed"
    );
}

#[cfg(windows)]
#[test]
fn remove_linked_worktree_windows_crlf_set_content() {
    let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
    let system_cfg = write_system_autocrlf_config(cfg_dir.path());

    let root = tempfile::tempdir().expect("tempdir");
    let main = root.path().join("m");
    std::fs::create_dir_all(&main).expect("mkdir main");
    git_with_system_config(&main, &system_cfg, &["init", "-q", "-b", "main"]);
    git_with_system_config(&main, &system_cfg, &["config", "user.email", "t@e.t"]);
    git_with_system_config(&main, &system_cfg, &["config", "user.name", "T"]);
    std::fs::write(main.join("a.txt"), b"a\n").expect("seed file");
    git_with_system_config(&main, &system_cfg, &["add", "a.txt"]);
    git_with_system_config(&main, &system_cfg, &["commit", "-qm", "one"]);

    let main_repo = open_grit_inherited_autocrlf(&main, &system_cfg);
    let linked = root.path().join("w");
    add_linked_worktree(
        &main_repo,
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some(WorktreeBranch::Create("f".to_owned())),
            start_point: Some("HEAD".to_owned()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("worktree add");

    std::fs::write(linked.join("new.txt"), b"new\r\n").expect("CRLF via bytes");
    let linked_repo = open_grit_inherited_autocrlf(&linked, &system_cfg);
    stage(&linked_repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(&linked_repo, &commit_request("two"), &mut NullProgress).expect("commit");

    remove_linked_worktree(&main_repo, &linked, false).expect("remove on Windows CRLF path");
}
