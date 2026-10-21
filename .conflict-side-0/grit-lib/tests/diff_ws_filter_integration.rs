//! Worktree diff with `-w` omits whitespace-only modified files.

use grit_lib::diff::{DiffOptions, DiffTarget, WhitespaceMode};
use grit_lib::repo::{InitOptions, Repository};
use std::process::Command;

fn git(cwd: &std::path::Path, args: &[&str]) {
    assert!(Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .unwrap()
        .success());
}

#[test]
fn worktree_diff_w_omits_whitespace_only_modified_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("ws.txt"), "hello world\n").unwrap();
    git(dir.path(), &["add", "ws.txt"]);
    git(dir.path(), &["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("ws.txt"), "helloworld\n").unwrap();

    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let opts = DiffOptions::default().with_whitespace(WhitespaceMode::ignore_all_space());
    let files = repo
        .diff()
        .files(&DiffTarget::WorktreeVsIndex, &[], &opts)
        .expect("diff");
    assert!(
        files.is_empty(),
        "whitespace-only unstaged change should produce no files: {:?}",
        files
    );
}
