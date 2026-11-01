//! Patch parsing vs `git apply --check` on git-generated unified diffs.

use std::process::Command;

use grit_lib::apply::parse_patch;
use tempfile::TempDir;

fn git_diff(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git diff");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn apply_parse_accepts_git_worktree_diff() {
    let dir = TempDir::new().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), b"one\n").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "a.txt"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "c"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), b"two\n").unwrap();
    let patch = git_diff(dir.path());
    assert!(!patch.is_empty(), "expected non-empty worktree diff");
    let files = parse_patch(&patch, 1, "patch", false, None).expect("parse");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].new_path.as_deref(), Some("a.txt"));
}
#[test]
fn mixed_text_and_binary_patch_writes_git_compatible_index() {
    use grit_lib::apply::{apply_patch_to_index_and_write, IndexApplyOptions};
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), b"before\n").unwrap();
    std::fs::write(root.join("b.bin"), b"before\0").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    std::fs::write(root.join("a.txt"), b"after\n").unwrap();
    std::fs::write(root.join("b.bin"), b"after\0").unwrap();
    let git_patch = git(root, &["diff", "--binary"]);
    git(root, &["add", "."]);
    let expected_tree = git(root, &["write-tree"]);
    git(root, &["reset", "--mixed", "HEAD"]);

    // Keep the text change as a traditional diff before the Git binary section.
    let (_, text_and_binary) = git_patch.split_once("--- a/a.txt\n").unwrap();
    let mixed_patch = format!("--- a/a.txt\n{text_and_binary}");
    let patches = parse_patch(&mixed_patch, 1, "mixed", false, None).expect("parse git diff");
    assert_eq!(patches.len(), 2);
    assert!(!patches[0].is_toplevel_relative);
    assert_eq!(patches[0].hunks.len(), 1);
    assert!(patches[1].is_toplevel_relative && patches[1].is_binary);

    let repo = Repository::discover(Some(root), &RepositoryOptions::default()).unwrap();
    apply_patch_to_index_and_write(
        &repo,
        &patches,
        &IndexApplyOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    assert_eq!(git(root, &["write-tree"]), expected_tree);
    assert_eq!(git(root, &["show", ":a.txt"]), "after\n");
    assert_eq!(git(root, &["show", ":b.bin"]), "after\0");
    git(root, &["fsck", "--strict"]);
}

#[cfg(unix)]
#[test]
fn quoted_patch_paths_roundtrip_through_git_index() {
    use grit_lib::apply::{apply_patch_to_index_and_write, IndexApplyOptions};
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diff::{write_patch, PatchTextOptions};
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let paths = [
        "tab\tname",
        "line\nname",
        "quote\"name",
        "café-東京",
        "back\\slash",
        "bell\x07name",
    ];
    for path in paths {
        std::fs::write(root.join(path), b"before\n").unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    for path in paths {
        std::fs::write(root.join(path), b"after\n").unwrap();
    }
    let patch = git(root, &["-c", "core.quotePath=true", "diff"]);
    let patches = parse_patch(&patch, 1, "quoted", false, None).expect("parse quoted git paths");
    assert_eq!(patches.len(), paths.len());
    for path in paths {
        assert!(
            patches.iter().any(|p| p.new_path.as_deref() == Some(path)),
            "{path:?}"
        );
    }
    git(root, &["add", "."]);
    let expected_tree = git(root, &["write-tree"]);
    git(root, &["reset", "--mixed", "HEAD"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::default()).unwrap();
    apply_patch_to_index_and_write(
        &repo,
        &patches,
        &IndexApplyOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    assert_eq!(git(root, &["write-tree"]), expected_tree);
    git(root, &["fsck", "--strict"]);

    // Have Git consume a grit-written patch for the same quoted paths.
    let old_tree =
        grit_lib::objects::ObjectId::from_hex(git(root, &["rev-parse", "HEAD^{tree}"]).trim())
            .unwrap();
    let new_tree = grit_lib::objects::ObjectId::from_hex(expected_tree.trim()).unwrap();
    let entries = repo
        .diff()
        .tree_to_tree(Some(&old_tree), Some(&new_tree))
        .unwrap();
    let options = PatchTextOptions::from_repo(&repo, Default::default()).unwrap();
    let mut grit_patch = Vec::new();
    write_patch(&repo, &entries, &options, &mut grit_patch).unwrap();
    git(root, &["reset", "--hard", "HEAD"]);
    let patch_path = dir.path().join("quoted.patch");
    std::fs::write(&patch_path, grit_patch).unwrap();
    git(root, &["apply", "--cached", "quoted.patch"]);
    assert_eq!(git(root, &["write-tree"]), expected_tree);
    git(root, &["fsck", "--strict"]);
}
