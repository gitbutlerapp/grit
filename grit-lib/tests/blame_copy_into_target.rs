//! Copy detection (`-CCC`) when lines are inserted into an existing file (#1610 review).
//!
//! Each case is cross-checked against `git blame -CCC` from the system `git`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use grit_lib::blame::{compute_blame, BlameDiffAlgorithm, BlameLine};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Copy Test")
        .env("GIT_AUTHOR_EMAIL", "copy@example.com")
        .env("GIT_COMMITTER_NAME", "Copy Test")
        .env("GIT_COMMITTER_EMAIL", "copy@example.com");
    cmd
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = git_command(dir).args(args).output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn git_run(dir: &Path, args: &[&str]) {
    git_stdout(dir, args);
}

fn head_oid(dir: &Path) -> ObjectId {
    git_stdout(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid")
}

/// `source.txt` is edited while `copied` lines are copied into existing `target.txt`.
fn build_copy_into_target_fixture(copied: [&str; 3]) -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.email", "copy@example.com"]);
    git_run(root, &["config", "user.name", "Copy Test"]);

    let [first, second, third] = copied;
    std::fs::write(
        root.join("source.txt"),
        format!("{first}\n{second}\n{third}\n"),
    )
    .unwrap();
    std::fs::write(root.join("target.txt"), "existing\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-q", "-m", "base"]);
    let base_oid = head_oid(root);

    std::fs::write(
        root.join("source.txt"),
        format!("modified\n{second}\n{third}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("target.txt"),
        format!("existing\n{first}\n{second}\n{third}\n"),
    )
    .unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-q", "-m", "copy"]);
    let tip = head_oid(root);

    (tmp, base_oid, tip)
}

fn grit_blame_ccc(root: &Path, tip: ObjectId) -> Vec<BlameLine> {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let grafts = HashMap::new();
    let policy = grit_lib::parallel::ThreadPolicy::default();
    compute_blame(
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
    .expect("compute_blame")
}

/// `(commit, source file)` per final line number from `git blame -CCC --line-porcelain`.
fn git_blame_ccc(root: &Path) -> HashMap<usize, (ObjectId, String)> {
    let porcelain = git_stdout(root, &["blame", "-CCC", "--line-porcelain", "target.txt"]);
    let mut out = HashMap::new();
    let mut current: Option<(ObjectId, usize)> = None;
    for line in porcelain.lines() {
        if line.starts_with('\t') {
            continue;
        }
        if let Some(path) = line.strip_prefix("filename ") {
            let (oid, final_line) = current.take().expect("header before filename");
            out.insert(final_line, (oid, path.to_owned()));
            continue;
        }
        let mut fields = line.split(' ');
        let Some(oid) = fields.next().and_then(|f| f.parse::<ObjectId>().ok()) else {
            continue;
        };
        let final_line = fields.nth(1).expect("final line").parse().expect("number");
        current = Some((oid, final_line));
    }
    out
}

fn line_at(lines: &[BlameLine], final_line: usize) -> &BlameLine {
    lines
        .iter()
        .find(|l| l.final_lineno == final_line)
        .expect("grit line")
}

#[test]
fn compute_blame_copy_into_existing_target_attributes_source_file() {
    let (tmp, base_oid, tip) = build_copy_into_target_fixture([
        "alpha line copied from the source file into target one",
        "beta line copied from the source file into target two",
        "gamma line copied from the source file into target three",
    ]);
    let root = tmp.path();
    let lines = grit_blame_ccc(root, tip);
    let git = git_blame_ccc(root);

    for final_line in 2..=4 {
        let grit_line = line_at(&lines, final_line);
        assert_ne!(
            grit_line.oid, tip,
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
            git.get(&final_line),
            Some(&(base_oid, "source.txt".to_owned())),
            "git blame -CCC agrees on final line {final_line}"
        );
    }
}

/// A copied chunk scoring below Git's copy threshold stays on the copying commit.
#[test]
fn compute_blame_copy_below_score_threshold_stays_on_copy_commit() {
    let (tmp, _base_oid, tip) = build_copy_into_target_fixture(["alpha", "beta", "gamma"]);
    let root = tmp.path();
    let lines = grit_blame_ccc(root, tip);
    let git = git_blame_ccc(root);

    for final_line in 2..=4 {
        let grit_line = line_at(&lines, final_line);
        assert_eq!(
            grit_line.oid, tip,
            "final line {final_line} stays on the copy commit"
        );
        assert_ne!(
            grit_line.source_file.as_deref(),
            Some("source.txt"),
            "final line {final_line} is not attributed to the source file"
        );
        assert_eq!(
            git.get(&final_line),
            Some(&(tip, "target.txt".to_owned())),
            "git blame -CCC agrees on final line {final_line}"
        );
    }
}
