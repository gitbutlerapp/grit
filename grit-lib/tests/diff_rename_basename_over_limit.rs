//! Basename rename pre-pass must pair unique 1:1 paths before renameLimit skips inexact detection.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{DiffStatus, RenameDetection, TreeDiffOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::forced_parallel_policy;

fn git_run(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Diff Test")
        .env("GIT_AUTHOR_EMAIL", "diff@example.com")
        .env("GIT_COMMITTER_NAME", "Diff Test")
        .env("GIT_COMMITTER_EMAIL", "diff@example.com")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse_tree(dir: &Path, rev: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("oid")
}

fn write_unique_basename_inexact_history(root: &Path, count: usize) {
    for i in 0..count {
        std::fs::create_dir_all(root.join("old")).unwrap();
        let mut body = format!("header {i}\n");
        for line in 1..=400 {
            body.push_str(&format!("line {line} file {i}\n"));
        }
        std::fs::write(root.join(format!("old/file{i}.txt")), body).unwrap();
    }
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "many"]);

    for i in 0..count {
        std::fs::remove_file(root.join(format!("old/file{i}.txt"))).unwrap();
        std::fs::create_dir_all(root.join("new")).unwrap();
        let mut body = format!("header {i}\n");
        for line in 1..=400 {
            body.push_str(&format!("line {line} file {i}\n"));
        }
        body.push_str("extra\n");
        std::fs::write(root.join(format!("new/file{i}.txt")), body).unwrap();
    }
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "moved"]);
}

fn git_rename_lines(root: &Path, parent: &ObjectId, commit: &ObjectId) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "diff-tree",
            "-r",
            "-M50",
            "--root",
            "--raw",
            &parent.to_hex(),
            &commit.to_hex(),
        ])
        .output()
        .expect("diff-tree");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with(':'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn inexact_unique_basenames_above_default_rename_limit_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);

    const N: usize = 1001;
    write_unique_basename_inexact_history(root, N);

    let parent = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit = rev_parse_tree(root, "HEAD^{tree}");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent), Some(&commit), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    let config = ConfigSet::new();
    let policy = forced_parallel_policy();
    let result = repo
        .diff()
        .detect_renames(
            raw,
            None,
            &TreeDiffOptions::default()
                .with_renames(RenameDetection::Similarity(50))
                .with_threads(Some(policy)),
        )
        .expect("detect renames");

    assert!(
        result.inexact_rename_skipped.is_none(),
        "basename pre-pass should shrink matrix below renameLimit=1000"
    );
    let renames = result
        .entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .count();
    assert_eq!(
        renames, N,
        "expected {N} inexact renames via basename pairing"
    );

    let git_lines = git_rename_lines(root, &parent, &commit);
    assert_eq!(git_lines.len(), N);
}
