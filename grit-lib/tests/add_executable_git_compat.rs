//! `grit add --executable` with `core.filemode=false` (issue #1043).

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
use grit_lib::porcelain::index_mode::StageExecutableHint;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn ident() -> String {
    "Exec Test <exec@example.com> 1700000000 +0000".to_owned()
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
        hooks: HookPolicy::default(),
        dry_run: false,
    }
}

#[test]
fn add_executable_stages_100755_when_filemode_false() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.filemode", "false"]);
    fs::write(root.join("tool.sh"), "#!/bin/sh\necho hi\n").expect("write");

    let repo = Repository::discover(Some(root), &Default::default()).expect("open");
    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["tool.sh".to_owned()],
            pathspec_sources: vec!["tool.sh".to_owned()],
            mode: StageMode::All,
            executable_hint: StageExecutableHint::SetExecutable,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage");
    assert_eq!(
        outcome.total(),
        1,
        "one new path must count as one staged change, got {outcome:?}"
    );

    let staged = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(
        staged.starts_with("100755 "),
        "index mode should be 100755, got: {staged}"
    );

    create_commit(&repo, &commit_req("script"), &mut NullProgress).expect("commit");
    let tree_mode = git_out(root, &["ls-tree", "HEAD", "tool.sh"]);
    assert!(
        tree_mode.starts_with("100755 blob"),
        "commit tree should be executable: {tree_mode}"
    );
}

#[test]
fn add_executable_mode_only_on_tracked_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.filemode", "false"]);
    fs::write(root.join("tool.sh"), "#!/bin/sh\n").expect("write");

    let repo = Repository::discover(Some(root), &Default::default()).expect("open");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["tool.sh".to_owned()],
            pathspec_sources: vec!["tool.sh".to_owned()],
            mode: StageMode::All,
            executable_hint: StageExecutableHint::None,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage");
    create_commit(&repo, &commit_req("base"), &mut NullProgress).expect("commit");

    let before = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(before.starts_with("100644 "), "baseline 644: {before}");

    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["tool.sh".to_owned()],
            pathspec_sources: vec!["tool.sh".to_owned()],
            mode: StageMode::All,
            executable_hint: StageExecutableHint::SetExecutable,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("chmod stage");
    assert_eq!(
        outcome.modified.len(),
        1,
        "mode-only update should stage one path"
    );

    let after = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(
        after.starts_with("100755 "),
        "expected 755 after hint: {after}"
    );

    create_commit(&repo, &commit_req("executable"), &mut NullProgress).expect("commit");
    let tree_line = git_out(root, &["ls-tree", "HEAD", "tool.sh"]);
    assert!(
        tree_line.starts_with("100755 blob"),
        "committed tree must be executable, not stale cache-tree 644: {tree_line}"
    );

    let fsck = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck --strict: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    let cached_diff = git_out(root, &["diff", "--cached"]);
    assert!(
        cached_diff.is_empty(),
        "index should match HEAD after commit, got staged diff: {cached_diff}"
    );
}

#[cfg(unix)]
#[test]
fn filemode_false_stages_regular_to_symlink_typechange() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.filemode", "false"]);
    fs::write(root.join("target.txt"), "link me\n").expect("write target");
    fs::write(root.join("path"), "regular\n").expect("write");

    let repo = Repository::discover(Some(root), &Default::default()).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(&repo, &commit_req("base"), &mut NullProgress).expect("commit");

    fs::remove_file(root.join("path")).expect("remove");
    symlink(root.join("target.txt"), root.join("path")).expect("symlink");

    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["path".to_owned()],
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage symlink");

    let grit_index = git_out(root, &["ls-files", "-s", "path"]);
    assert!(
        grit_index.starts_with("120000 "),
        "grit must stage symlink typechange as 120000: {grit_index}"
    );

    let git_repo = tempfile::tempdir().expect("git copy");
    let gr = git_repo.path();
    git(gr, &["init", "-q", "-b", "main"]);
    git(gr, &["config", "core.filemode", "false"]);
    fs::write(gr.join("target.txt"), "link me\n").expect("write");
    fs::write(gr.join("path"), "regular\n").expect("write");
    git(gr, &["add", "-A"]);
    git(gr, &["commit", "-qm", "base"]);
    fs::remove_file(gr.join("path")).expect("remove");
    symlink(gr.join("target.txt"), gr.join("path")).expect("symlink");
    git(gr, &["add", "path"]);
    let git_index = git_out(gr, &["ls-files", "-s", "path"]);
    assert!(
        git_index.starts_with("120000 "),
        "git reference: {git_index}"
    );
}

#[test]
fn add_does_not_clobber_existing_executable_in_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.filemode", "false"]);
    fs::write(root.join("tool.sh"), "v1\n").expect("write");

    let repo = Repository::discover(Some(root), &Default::default()).expect("open");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["tool.sh".to_owned()],
            pathspec_sources: vec!["tool.sh".to_owned()],
            mode: StageMode::All,
            executable_hint: StageExecutableHint::SetExecutable,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage");
    let staged = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(staged.starts_with("100755 "));

    fs::write(root.join("tool.sh"), "v2\n").expect("write");
    stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["tool.sh".to_owned()],
            pathspec_sources: vec!["tool.sh".to_owned()],
            mode: StageMode::All,
            executable_hint: StageExecutableHint::None,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("restage");
    let restaged = git_out(root, &["ls-files", "-s", "tool.sh"]);
    assert!(
        restaged.starts_with("100755 "),
        "content refresh must keep explicit 755: {restaged}"
    );
}
