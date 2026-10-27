//! Copy detection (`-CCC`) when lines are inserted into an existing file (#1610 review).

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
        .env("GIT_AUTHOR_NAME", "Copy Test")
        .env("GIT_AUTHOR_EMAIL", "copy@example.com")
        .env("GIT_COMMITTER_NAME", "Copy Test")
        .env("GIT_COMMITTER_EMAIL", "copy@example.com")
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

/// Copied lines, each long enough (at least 40 alphanumeric characters, Git's default
/// `blame` copy score) for copy detection to accept the moved chunk.
const ALPHA: &str = "alpha: the first copied line, long enough to score well\n";
const BETA: &str = "beta: the second copied line, long enough to score well\n";
const GAMMA: &str = "gamma: the third copied line, long enough to score well\n";

/// `source.txt` is edited while three lines are copied into existing `target.txt`.
fn build_copy_into_target_fixture() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.email", "copy@example.com"]);
    git_run(root, &["config", "user.name", "Copy Test"]);

    std::fs::write(root.join("source.txt"), format!("{ALPHA}{BETA}{GAMMA}")).unwrap();
    std::fs::write(root.join("target.txt"), "existing\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-q", "-m", "base"]);
    let base_oid = head_oid(root);

    std::fs::write(root.join("source.txt"), format!("modified\n{BETA}{GAMMA}")).unwrap();
    std::fs::write(
        root.join("target.txt"),
        format!("existing\n{ALPHA}{BETA}{GAMMA}"),
    )
    .unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-q", "-m", "copy"]);
    let tip = head_oid(root);

    (tmp, base_oid, tip)
}

#[test]
fn compute_blame_copy_into_existing_target_attributes_source_file() {
    let (_tmp, base_oid, tip) = build_copy_into_target_fixture();
    let root = _tmp.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let grafts = HashMap::new();
    let policy = grit_lib::parallel::ThreadPolicy::default();

    let lines = compute_blame(
        repo.odb(),
        tip,
        "target.txt",
        &HashSet::new(),
        BlameDiffAlgorithm::Myers,
        None,
        false,
        3,
        false,
        &grafts,
        true,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &policy,
    )
    .expect("compute_blame");

    let git_blame = git_blame_porcelain_ccc(root, "target.txt");
    let copy_oid = tip;
    for final_line in 2..=4 {
        let grit_line = lines
            .iter()
            .find(|l| l.final_lineno == final_line)
            .expect("grit line");
        assert_ne!(
            grit_line.oid, copy_oid,
            "final line {final_line} must not stop on the copy commit"
        );
        assert_eq!(
            grit_line.oid, base_oid,
            "final line {final_line} attributes to source history"
        );
        assert_eq!(
            grit_line.source_file.as_deref(),
            Some("source.txt"),
            "final line {final_line} source path"
        );
        assert_eq!(
            (grit_line.oid, "source.txt"),
            (
                git_blame[final_line - 1].0,
                git_blame[final_line - 1].1.as_str()
            ),
            "final line {final_line} matches git blame -CCC"
        );
    }
}

/// `(commit, filename)` per final line from `git blame -CCC --line-porcelain`.
fn git_blame_porcelain_ccc(dir: &Path, path: &str) -> Vec<(ObjectId, String)> {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["blame", "-CCC", "--line-porcelain", path])
        .output()
        .expect("git blame");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    let mut commit = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("filename ") {
            rows.push((commit.take().expect("commit header"), name.to_owned()));
        } else if !line.starts_with('\t') {
            if let Some(first) = line.split(' ').next() {
                if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
                    commit = Some(first.parse().expect("oid"));
                }
            }
        }
    }
    rows
}
