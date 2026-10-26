//! Stable public signature for [`grit_lib::blame::compute_blame`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use grit_lib::blame::{compute_blame, BlameDiffAlgorithm};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_run(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Blame API Test")
        .env("GIT_AUTHOR_EMAIL", "blame-api@example.com")
        .env("GIT_COMMITTER_NAME", "Blame API Test")
        .env("GIT_COMMITTER_EMAIL", "blame-api@example.com")
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

fn head_oid(dir: &Path) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("oid")
}

#[test]
fn compute_blame_public_entry_point_attributes_lines() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f.txt"), "one\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "init"]);
    std::fs::write(root.join("f.txt"), "one\ntwo\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "add second line"]);

    let start = head_oid(root);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let ignore_revs = HashSet::new();
    let grafts = HashMap::new();

    let lines = compute_blame(
        repo.odb(),
        start,
        "f.txt",
        &ignore_revs,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        &grafts,
        false,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &grit_lib::parallel::ThreadPolicy::default(),
    )
    .expect("compute_blame");

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].final_lineno, 1);
    assert_eq!(lines[1].final_lineno, 2);
    assert_ne!(
        lines[0].oid, lines[1].oid,
        "second line should be a newer commit"
    );
}

#[test]
fn compute_blame_line_range_limits_output_and_work() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f.txt"), "stable\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "intro"]);
    let root_commit = head_oid(root);
    for i in 0..80 {
        std::fs::write(root.join("other.txt"), format!("noise {i}\n")).unwrap();
        git_run(root, &["add", "other.txt"]);
        git_run(root, &["commit", "-m", &format!("touch {i}")]);
    }

    let start = head_oid(root);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let ignore_revs = HashSet::new();
    let grafts = HashMap::new();
    let policy = grit_lib::parallel::ThreadPolicy::default();

    let ranged = compute_blame(
        repo.odb(),
        start,
        "f.txt",
        &ignore_revs,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        &grafts,
        false,
        Some(1..=1),
        grit_lib::diff::WhitespaceMode::default(),
        &policy,
    )
    .expect("ranged blame");

    assert_eq!(ranged.len(), 1);
    assert_eq!(ranged[0].final_lineno, 1);
    assert_eq!(ranged[0].oid, root_commit);
}

#[test]
fn compute_blame_line_range_through_ignored_merge_after_line_insert() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root_dir = tmp.path();
    git_run(root_dir, &["init", "-q", "-b", "main"]);
    git_run(root_dir, &["config", "user.name", "Blame API Test"]);
    git_run(root_dir, &["config", "user.email", "blame-api@example.com"]);

    std::fs::write(root_dir.join("target.txt"), "target\n").unwrap();
    git_run(root_dir, &["add", "target.txt"]);
    git_run(root_dir, &["commit", "-m", "root target line"]);
    let root_commit = head_oid(root_dir);

    git_run(root_dir, &["branch", "side"]);
    git_run(root_dir, &["checkout", "side"]);
    std::fs::write(root_dir.join("side.txt"), "side\n").unwrap();
    git_run(root_dir, &["add", "side.txt"]);
    git_run(root_dir, &["commit", "-m", "side branch tip"]);

    git_run(root_dir, &["checkout", "main"]);
    git_run(
        root_dir,
        &["merge", "--no-ff", "side", "-m", "ignored merge"],
    );
    let merge_commit = head_oid(root_dir);

    std::fs::write(root_dir.join("target.txt"), "new top\ntarget\n").unwrap();
    git_run(root_dir, &["add", "target.txt"]);
    git_run(root_dir, &["commit", "-m", "insert line above target"]);
    let tip = head_oid(root_dir);

    let git_out = Command::new("git")
        .current_dir(root_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "blame",
            "--porcelain",
            &format!("--ignore-rev={merge_commit}"),
            "-L",
            "2,2",
            "target.txt",
        ])
        .output()
        .expect("git blame");
    assert!(
        git_out.status.success(),
        "git blame failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let git_sha = git_stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .expect("git porcelain sha");
    assert_eq!(git_sha, root_commit.to_hex());

    let repo = Repository::discover(Some(root_dir), &RepositoryOptions::empty()).expect("open");
    let mut ignore_revs = HashSet::new();
    ignore_revs.insert(merge_commit);
    let grafts = HashMap::new();
    let policy = grit_lib::parallel::ThreadPolicy::default();

    let full = compute_blame(
        repo.odb(),
        tip,
        "target.txt",
        &ignore_revs,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        &grafts,
        false,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &policy,
    )
    .expect("full blame");
    let full_line2 = full
        .iter()
        .find(|l| l.final_lineno == 2)
        .expect("line 2 in full blame");
    assert_eq!(full_line2.oid, root_commit);

    let ranged = compute_blame(
        repo.odb(),
        tip,
        "target.txt",
        &ignore_revs,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        &grafts,
        false,
        Some(2..=2),
        grit_lib::diff::WhitespaceMode::default(),
        &policy,
    )
    .expect("ranged blame");
    assert_eq!(ranged.len(), 1);
    assert_eq!(ranged[0].final_lineno, 2);
    assert_eq!(
        ranged[0].oid, root_commit,
        "ranged blame must match full blame and git through ignored merge"
    );
}

#[test]
fn compute_blame_final_newline_changes_preserve_bodies_and_ranges() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    let mut first_commit = None;
    for text in ["first\nhello", "first\nhello\n", "first\nhello"] {
        std::fs::write(root.join("a.txt"), text).unwrap();
        git_run(root, &["add", "a.txt"]);
        git_run(root, &["commit", "-qm", "change ending"]);
        let modified = head_oid(root);
        let first = *first_commit.get_or_insert(modified);
        git_run(root, &["commit", "--allow-empty", "-qm", "unrelated"]);
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        for range in [None, Some(2..=2)] {
            let out = Command::new("git")
                .current_dir(root)
                .args(["blame", "--line-porcelain", "--", "a.txt"])
                .output()
                .unwrap();
            assert!(out.status.success());
            let oracle: Vec<_> = String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| {
                    let parts: Vec<_> = line.split_whitespace().collect();
                    (parts.len() >= 3 && parts[0].len() == 40)
                        .then(|| parts[0].parse::<ObjectId>().unwrap())
                })
                .collect();
            let lines = compute_blame(
                repo.odb(),
                head_oid(root),
                "a.txt",
                &HashSet::new(),
                BlameDiffAlgorithm::Myers,
                None,
                false,
                0,
                false,
                &HashMap::new(),
                true,
                range.clone(),
                grit_lib::diff::WhitespaceMode::default(),
                &grit_lib::parallel::ThreadPolicy::default(),
            )
            .unwrap();
            assert_eq!(oracle, vec![first, modified]);
            assert_eq!(lines.len(), if range.is_some() { 1 } else { 2 });
            for line in lines {
                assert_eq!(line.oid, oracle[line.final_lineno - 1]);
                assert_eq!(line.orig_lineno, line.final_lineno);
                assert_eq!(
                    line.content,
                    if line.final_lineno == 1 {
                        "first"
                    } else {
                        "hello"
                    }
                );
            }
        }
    }
    git_run(root, &["fsck", "--strict"]);
}
