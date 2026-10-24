//! `restore_hunks` on the worktree, checked against system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide, HunkId};
use grit_lib::porcelain::hunk_select::{restore_hunks, unstaged_file_diffs, HunkSelection};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_status_ok(repo: &Path, args: &[&str], stdin: Option<&[u8]>) -> (bool, Vec<u8>) {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("git output");
    (out.status.success(), out.stdout)
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    dir
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn loose_object_count(root: &Path) -> usize {
    fs::read_dir(root.join(".git/objects"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().len() == 2)
        .map(|e| fs::read_dir(e.path()).unwrap().count())
        .sum()
}

fn unstaged_ids(repo: &Repository, path: &str) -> Vec<HunkId> {
    let pathspec = vec![path.to_string()];
    let diffs = unstaged_file_diffs(repo, &pathspec).expect("unstaged diffs");
    let (file, peer) = diffs.first().expect("path is modified");
    hunk_ids_with_duplicate_peers(DiffSide::Unstaged, path, &file.hunks, peer)
}

fn restore(repo: &Repository, ids: Vec<HunkId>, path: &str) {
    let selection = HunkSelection {
        ids,
        ranges: Vec::new(),
    };
    restore_hunks(repo, &selection, &[path.to_string()], false, true, false)
        .expect("restore hunks");
}

#[test]
fn restoring_half_of_many_hunks_matches_git_apply_reverse() {
    let dir = init_repo();
    let root = dir.path();
    let original: Vec<String> = (0..4_000).map(|i| format!("line{i}")).collect();
    fs::write(root.join("big.txt"), original.join("\n") + "\n").unwrap();
    git(root, &["add", "big.txt"]);
    git(root, &["commit", "-qm", "base"]);

    let mut edited = original.clone();
    for h in 0..40 {
        edited[h * 100] = format!("LINE{}", h * 100);
    }
    fs::write(root.join("big.txt"), edited.join("\n") + "\n").unwrap();

    let repo = open(root);
    let ids = unstaged_ids(&repo, "big.txt");
    assert_eq!(ids.len(), 40);
    let objects_before = loose_object_count(root);
    restore(&repo, ids[..20].to_vec(), "big.txt");

    let mut expected = original;
    for h in 20..40 {
        expected[h * 100] = format!("LINE{}", h * 100);
    }
    assert_eq!(
        fs::read_to_string(root.join("big.txt")).unwrap(),
        expected.join("\n") + "\n"
    );
    assert_eq!(loose_object_count(root), objects_before);

    let remaining = git(root, &["diff", "--numstat", "--", "big.txt"]);
    assert_eq!(remaining.trim(), "20\t20\tbig.txt");
    let (fsck_ok, _) = git_status_ok(root, &["fsck", "--strict"], None);
    assert!(fsck_ok, "git fsck --strict");

    let reopened = open(root);
    assert_eq!(unstaged_ids(&reopened, "big.txt").len(), 20);
}

#[test]
fn restoring_every_hunk_leaves_a_clean_index_entry() {
    let dir = init_repo();
    let root = dir.path();
    fs::write(root.join("f.txt"), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n").unwrap();
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);
    fs::write(root.join("f.txt"), "A\nb\nc\nd\ne\nf\ng\nh\ni\nJ").unwrap();

    let repo = open(root);
    let ids = unstaged_ids(&repo, "f.txt");
    assert_eq!(ids.len(), 2);
    restore(&repo, ids, "f.txt");

    assert_eq!(
        fs::read_to_string(root.join("f.txt")).unwrap(),
        "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n"
    );
    assert_eq!(git(root, &["status", "--porcelain"]).trim(), "");
}

#[test]
fn ident_expansion_uses_the_restored_content_id_without_storing_it() {
    let dir = init_repo();
    let root = dir.path();
    fs::write(root.join(".gitattributes"), "*.txt ident\n").unwrap();
    let base = "$Id$\none\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
    fs::write(root.join("id.txt"), base).unwrap();
    git(root, &["add", ".gitattributes", "id.txt"]);
    git(root, &["commit", "-qm", "base"]);

    let edited = fs::read_to_string(root.join("id.txt"))
        .unwrap()
        .replace("one\n", "ONE\n")
        .replace("ten\n", "TEN\n");
    fs::write(root.join("id.txt"), edited).unwrap();

    let repo = open(root);
    let ids = unstaged_ids(&repo, "id.txt");
    assert_eq!(ids.len(), 2);
    restore(&repo, ids[..1].to_vec(), "id.txt");

    let clean = base.replace("ten\n", "TEN\n");
    let (hashed, stdout) = git_status_ok(root, &["hash-object", "--stdin"], Some(clean.as_bytes()));
    assert!(hashed);
    let oid = String::from_utf8(stdout).unwrap().trim().to_string();
    assert_eq!(
        fs::read_to_string(root.join("id.txt")).unwrap(),
        clean.replace("$Id$", &format!("$Id: {oid} $"))
    );
    let (stored, _) = git_status_ok(root, &["cat-file", "-e", &oid], None);
    assert!(!stored, "restored content must not be written as a blob");
    assert_eq!(
        git(root, &["diff", "--numstat", "--", "id.txt"]).trim(),
        "1\t1\tid.txt"
    );
}
