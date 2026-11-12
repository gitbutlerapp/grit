//! Serial vs parallel rename/copy detection must match exactly.

use grit_lib::objects::FileMode;
use std::path::Path;
use std::process::Command;

use grit_lib::blame::{compute_blame, BlameDiffAlgorithm};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    zero_oid, CopyDetection, DiffEntry, DiffStatus, RenameDetection, TreeDiffOptions,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};
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

fn serial_policy() -> ThreadPolicy {
    ThreadPolicy::default()
        .with_threads_override(Some(1))
        .with_min_items_override(Some(0))
        .with_min_total_bytes_override(Some(0))
}

fn parallel_policy() -> ThreadPolicy {
    forced_parallel_policy()
}

fn rename_options(threshold: u8, policy: ThreadPolicy) -> TreeDiffOptions {
    TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(threshold))
        .with_threads(Some(policy))
}

fn copy_options(threshold: u8, policy: ThreadPolicy) -> TreeDiffOptions {
    TreeDiffOptions::default()
        .with_copies(CopyDetection::Similarity(threshold))
        .with_threads(Some(policy))
}

fn assert_serial_parallel_rename_match(repo: &Repository, entries: Vec<DiffEntry>, threshold: u8) {
    let serial = repo
        .diff()
        .detect_renames(
            entries.clone(),
            None,
            &rename_options(threshold, serial_policy()),
        )
        .expect("serial rename detection")
        .entries;
    let parallel = repo
        .diff()
        .detect_renames(entries, None, &rename_options(threshold, parallel_policy()))
        .expect("parallel rename detection")
        .entries;
    assert_eq!(
        serial, parallel,
        "parallel rename detection must match serial output"
    );
}

fn git_name_status(dir: &Path, old_tree: &ObjectId, new_tree: &ObjectId) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "diff-tree",
            "-M50",
            "-C50",
            "--name-status",
            "--root",
            "-r",
            &old_tree.to_hex(),
            &new_tree.to_hex(),
        ])
        .output()
        .expect("diff-tree");
    assert!(
        out.status.success(),
        "git diff-tree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn rename_and_copy_lines(entries: &[DiffEntry]) -> Vec<String> {
    let mut lines: Vec<String> = entries
        .iter()
        .filter(|e| matches!(e.status, DiffStatus::Renamed | DiffStatus::Copied))
        .map(|e| {
            let score = e.score.unwrap_or(100);
            let tag = match e.status {
                DiffStatus::Renamed => format!("R{score:03}"),
                DiffStatus::Copied => format!("C{score:03}"),
                _ => String::new(),
            };
            format!(
                "{tag}\t{}\t{}",
                e.old_path.as_deref().unwrap_or(""),
                e.new_path.as_deref().unwrap_or("")
            )
        })
        .collect();
    lines.sort();
    lines
}

fn git_rename_and_copy_lines(dir: &Path, old_tree: &ObjectId, new_tree: &ObjectId) -> Vec<String> {
    let raw = git_name_status(dir, old_tree, new_tree);
    let mut lines: Vec<String> = raw
        .lines()
        .filter(|line| line.starts_with('R') || line.starts_with('C') || line.starts_with('M'))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

#[test]
fn parallel_rename_matches_serial_exact_and_inexact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);

    std::fs::write(root.join("exact.txt"), b"same content\n").unwrap();
    let body: String = (1..=30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(root.join("inexact.txt"), format!("{body}\n")).unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "base"]);

    std::fs::write(root.join("exact.txt"), b"same content\n").unwrap();
    let mut edited = body;
    edited = edited.replacen("line 10", "line TEN", 1);
    std::fs::remove_file(root.join("inexact.txt")).unwrap();
    std::fs::write(root.join("exact-renamed.txt"), b"same content\n").unwrap();
    std::fs::write(root.join("inexact-renamed.txt"), format!("{edited}\n")).unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "renames"]);

    let parent = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit = rev_parse_tree(root, "HEAD^{tree}");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent), Some(&commit), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    assert_serial_parallel_rename_match(&repo, raw.clone(), 50);

    let detected = repo
        .diff()
        .detect_renames(raw, None, &rename_options(50, parallel_policy()))
        .expect("detect renames")
        .entries;
    let git_lines = git_rename_and_copy_lines(root, &parent, &commit);
    let grit_lines = rename_and_copy_lines(&detected);
    assert_eq!(
        git_lines, grit_lines,
        "rename/copy paths and scores should match git diff-tree -M50"
    );
}

#[test]
fn exact_renames_above_default_rename_limit_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);

    const N: usize = 1001;
    for i in 0..N {
        std::fs::write(root.join(format!("old{i}.txt")), format!("payload {i}\n")).unwrap();
    }
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "many"]);

    for i in 0..N {
        std::fs::remove_file(root.join(format!("old{i}.txt"))).unwrap();
        std::fs::write(root.join(format!("new{i}.txt")), format!("payload {i}\n")).unwrap();
    }
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "renamed"]);

    let parent = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit = rev_parse_tree(root, "HEAD^{tree}");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent), Some(&commit), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    let detected = repo
        .diff()
        .detect_renames(raw, None, &rename_options(50, parallel_policy()))
        .expect("detect renames")
        .entries;

    let renames = detected
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .count();
    assert_eq!(
        renames, N,
        "expected {N} exact renames under renameLimit=1000"
    );
    assert_eq!(
        detected
            .iter()
            .filter(|e| e.status == DiffStatus::Deleted)
            .count(),
        0
    );
    assert_eq!(
        detected
            .iter()
            .filter(|e| e.status == DiffStatus::Added)
            .count(),
        0
    );

    let git_lines = git_rename_and_copy_lines(root, &parent, &commit);
    let grit_lines = rename_and_copy_lines(&detected);
    assert_eq!(git_lines.len(), N);
    assert_eq!(git_lines, grit_lines);
}

#[test]
fn asymmetric_matrix_exact_matches_still_pair() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let shared = b"shared exact payload\n";
    let oid = odb.write(ObjectKind::Blob, shared).expect("write");

    let mut entries = Vec::new();
    for i in 0..5 {
        entries.push(DiffEntry::new(
            DiffStatus::Deleted,
            Some(format!("old{i}.txt")),
            None,
            FileMode::REGULAR,
            FileMode::ABSENT,
            oid,
            zero_oid(),
        ));
    }
    for i in 0..50 {
        entries.push(DiffEntry::new(
            DiffStatus::Added,
            None,
            Some(format!("new{i}.txt")),
            FileMode::ABSENT,
            FileMode::REGULAR,
            zero_oid(),
            oid,
        ));
    }

    let detected = repo
        .diff()
        .detect_renames(entries, None, &rename_options(50, serial_policy()))
        .expect("detect renames")
        .entries;
    assert_eq!(
        detected
            .iter()
            .filter(|e| e.status == DiffStatus::Renamed)
            .count(),
        5
    );
}

#[test]
fn copy_mixed_exact_inexact_respects_rename_limit_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "diff.renameLimit", "3"]);

    let exact_a = b"exact payload alpha\n";
    let exact_b = b"exact payload beta\n";
    let body_c: String = (1..=40)
        .map(|i| format!("inexact-c line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body_d: String = (1..=40)
        .map(|i| format!("inexact-d line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(root.join("exact-old-a.txt"), exact_a).unwrap();
    std::fs::write(root.join("exact-old-b.txt"), exact_b).unwrap();
    std::fs::write(root.join("inexact-old-c.txt"), format!("{body_c}\n")).unwrap();
    std::fs::write(root.join("inexact-old-d.txt"), format!("{body_d}\n")).unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "base"]);

    let mut edited_c = format!("{body_c}\n");
    edited_c = edited_c.replacen("inexact-c line 10", "inexact-c line TEN", 1);
    let mut edited_d = format!("{body_d}\n");
    edited_d = edited_d.replacen("inexact-d line 20", "inexact-d line TWENTY", 1);

    for name in [
        "exact-old-a.txt",
        "exact-old-b.txt",
        "inexact-old-c.txt",
        "inexact-old-d.txt",
    ] {
        std::fs::remove_file(root.join(name)).unwrap();
    }
    std::fs::write(root.join("exact-new-a.txt"), exact_a).unwrap();
    std::fs::write(root.join("exact-new-b.txt"), exact_b).unwrap();
    std::fs::write(root.join("inexact-new-c.txt"), &edited_c).unwrap();
    std::fs::write(root.join("inexact-new-d.txt"), &edited_d).unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "renames"]);

    let parent = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit = rev_parse_tree(root, "HEAD^{tree}");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent), Some(&commit), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    let detected = repo
        .diff()
        .detect_renames(raw, Some(&parent), &copy_options(50, parallel_policy()))
        .expect("detect copies")
        .entries;

    let git_lines = git_rename_and_copy_lines(root, &parent, &commit);
    let grit_lines = rename_and_copy_lines(&detected);
    assert_eq!(
        git_lines, grit_lines,
        "mixed exact/inexact copy detection with renameLimit=3 must match git diff-tree -M50 -C50"
    );
    assert_eq!(
        git_lines.len(),
        4,
        "expected four rename lines: {git_lines:?}"
    );
    assert!(
        git_lines.iter().filter(|l| l.starts_with("R100")).count() >= 2,
        "expected two exact renames: {git_lines:?}"
    );
    assert!(
        git_lines
            .iter()
            .any(|l| l.starts_with("R0") && !l.starts_with("R100")),
        "expected inexact rename scores: {git_lines:?}"
    );
}

#[test]
fn parallel_copy_many_to_one_matches_serial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);

    let shared = b"shared block one\nshared block two\nunique tail\n";
    std::fs::write(root.join("source.txt"), shared).unwrap();
    git_run(root, &["add", "source.txt"]);
    git_run(root, &["commit", "-m", "base"]);

    std::fs::remove_file(root.join("source.txt")).unwrap();
    for name in ["copy-a.txt", "copy-b.txt", "copy-c.txt"] {
        std::fs::write(root.join(name), shared).unwrap();
    }
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "copies"]);

    let parent = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit = rev_parse_tree(root, "HEAD^{tree}");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent), Some(&commit), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    let serial = repo
        .diff()
        .detect_renames(
            raw.clone(),
            Some(&parent),
            &copy_options(50, serial_policy()),
        )
        .expect("detect copies")
        .entries;
    let parallel = repo
        .diff()
        .detect_renames(raw, Some(&parent), &copy_options(50, parallel_policy()))
        .expect("detect copies")
        .entries;
    assert_eq!(
        serial, parallel,
        "parallel copy detection must match serial"
    );
    assert!(
        serial.iter().any(|e| e.status == DiffStatus::Renamed),
        "one destination should be rename: {serial:?}"
    );
    assert!(
        serial
            .iter()
            .filter(|e| e.status == DiffStatus::Copied)
            .count()
            >= 2,
        "remaining destinations should be copies: {serial:?}"
    );
}

#[test]
fn blame_copy_serial_matches_parallel_scoring() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);

    let shared = "alpha line\nbeta line\ngamma line\n";
    std::fs::write(root.join("source.txt"), shared).unwrap();
    git_run(root, &["add", "source.txt"]);
    git_run(root, &["commit", "-m", "source"]);

    std::fs::write(root.join("target.txt"), shared).unwrap();
    git_run(root, &["add", "target.txt"]);
    git_run(root, &["commit", "-m", "target"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let start_commit = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    let start_oid: ObjectId = String::from_utf8_lossy(&start_commit.stdout)
        .trim()
        .parse()
        .expect("oid");
    let grafts = std::collections::HashMap::new();
    let ignore = std::collections::HashSet::new();

    let serial = compute_blame(
        repo.odb(),
        start_oid,
        "target.txt",
        &ignore,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        3,
        false,
        false,
        &grafts,
        false,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &serial_policy(),
    )
    .expect("serial blame");
    let parallel = compute_blame(
        repo.odb(),
        start_oid,
        "target.txt",
        &ignore,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        3,
        false,
        false,
        &grafts,
        false,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &parallel_policy(),
    )
    .expect("parallel blame");
    assert_eq!(
        serial, parallel,
        "blame -C scoring must match across thread counts"
    );
}

#[test]
fn parallel_rename_matrix_with_many_pairs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    for i in 0..32 {
        std::fs::write(root.join(format!("old{i}.txt")), format!("payload {i}\n")).unwrap();
    }
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "many"]);

    for i in 0..32 {
        let _ = std::fs::remove_file(root.join(format!("old{i}.txt")));
        std::fs::write(root.join(format!("new{i}.txt")), format!("payload {i}\n")).unwrap();
    }
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "many renames"]);

    let parent = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit = rev_parse_tree(root, "HEAD^{tree}");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let raw = repo
        .diff()
        .tree_to_tree(Some(&parent), Some(&commit), &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff");
    assert_serial_parallel_rename_match(&repo, raw, 50);
}
