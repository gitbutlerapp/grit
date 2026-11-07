//! `restore_hunks` on the worktree, checked against system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{hunk_ids_with_duplicate_peers, DiffSide, HunkId};
use grit_lib::porcelain::hunk_select::HunkSelection;
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
    let diffs = repo
        .index()
        .unstaged_file_diffs(&pathspec)
        .expect("unstaged diffs");
    let (file, peer) = diffs.first().expect("path is modified");
    hunk_ids(DiffSide::Unstaged, path, &file.hunks, peer)
}

fn restore(repo: &Repository, ids: Vec<HunkId>, path: &str) {
    let selection = HunkSelection {
        ids,
        ranges: Vec::new(),
    };
    repo.index()
        .restore_hunks(&selection, &[path.to_string()], false, true, false)
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

fn commit_with_edits(root: &Path, path: &str, lines: usize, every: usize) -> (String, String) {
    let original: String = (0..lines).map(|i| format!("line{i}\n")).collect();
    fs::write(root.join(path), &original).unwrap();
    git(root, &["add", path]);
    git(root, &["commit", "-qm", "base"]);
    let edited: String = (0..lines)
        .map(|i| {
            if i % every == 0 {
                format!("LINE{i}\n")
            } else {
                format!("line{i}\n")
            }
        })
        .collect();
    fs::write(root.join(path), &edited).unwrap();
    (original, edited)
}

/// `(size, mtime seconds)` git records for `path` in the index, read without a refresh.
fn index_stat(root: &Path, path: &str) -> (u64, u64) {
    let debug = git(root, &["ls-files", "--debug", "--", path]);
    let field = |name: &str| -> u64 {
        let line = debug
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("{name} in {debug}"));
        let value = line[name.len()..].trim();
        value
            .split(|c: char| c == ':' || c.is_whitespace())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    (field("size:"), field("mtime:"))
}

#[test]
fn partial_restore_leaves_the_index_file_untouched() {
    let dir = init_repo();
    let root = dir.path();
    commit_with_edits(root, "f.txt", 400, 50);
    // Restore in a later second than the commit so the entry is not racily clean: a racy entry
    // whose stat still matches is invalidated, and that change is written.
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let index_path = root.join(".git/index");
    let index_before = fs::read(&index_path).unwrap();
    let inode_before = fs::metadata(&index_path).unwrap();

    let repo = open(root);
    let ids = unstaged_ids(&repo, "f.txt");
    assert_eq!(ids.len(), 8);
    restore(&repo, ids[..4].to_vec(), "f.txt");

    assert_eq!(fs::read(&index_path).unwrap(), index_before);
    let inode_after = fs::metadata(&index_path).unwrap();
    assert_eq!(
        inode_after.modified().unwrap(),
        inode_before.modified().unwrap(),
        "a restore that leaves the file modified must not rewrite the index"
    );
    assert!(!root.join(".git/index.lock").exists());
    assert_eq!(
        git(root, &["diff", "--numstat", "--", "f.txt"]).trim(),
        "4\t4\tf.txt"
    );
    let (fsck_ok, _) = git_status_ok(root, &["fsck", "--strict"], None);
    assert!(fsck_ok, "git fsck --strict");
}

#[test]
fn full_restore_writes_refreshed_stat_data_git_reads() {
    let dir = init_repo();
    let root = dir.path();
    let (original, _) = commit_with_edits(root, "f.txt", 400, 50);
    // Make the recorded stat data stale even for an identical size.
    std::thread::sleep(std::time::Duration::from_millis(1_100));

    let repo = open(root);
    let ids = unstaged_ids(&repo, "f.txt");
    restore(&repo, ids, "f.txt");

    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), original);
    let meta = fs::metadata(root.join("f.txt")).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(index_stat(root, "f.txt"), (meta.len(), mtime));
    assert!(!root.join(".git/index.lock").exists());
    assert_eq!(git(root, &["diff-files", "--name-only"]).trim(), "");
}

#[test]
fn held_index_lock_fails_before_touching_the_worktree() {
    let dir = init_repo();
    let root = dir.path();
    let (_, edited) = commit_with_edits(root, "f.txt", 200, 50);
    let repo = open(root);
    let ids = unstaged_ids(&repo, "f.txt");
    fs::write(root.join(".git/index.lock"), b"").unwrap();

    let selection = HunkSelection {
        ids: ids[..1].to_vec(),
        ranges: Vec::new(),
    };
    let result = restore_hunks(
        &repo,
        &selection,
        &["f.txt".to_string()],
        false,
        true,
        false,
    );
    assert!(result.is_err(), "restore must not run under a held lock");
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), edited);
    assert!(
        root.join(".git/index.lock").exists(),
        "another writer's lock is left in place"
    );

    let dry = repo
        .index()
        .restore_hunks(&selection, &["f.txt".to_string()], false, true, true)
        .expect("dry run needs no lock");
    assert_eq!(dry.restored, vec!["f.txt".to_string()]);
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), edited);
}

#[test]
fn restoring_the_last_remaining_hunk_refreshes_the_entry() {
    let dir = init_repo();
    let root = dir.path();
    let (original, _) = commit_with_edits(root, "f.txt", 300, 100);
    std::thread::sleep(std::time::Duration::from_millis(1_100));

    for remaining in (1..=3).rev() {
        let repo = open(root);
        let ids = unstaged_ids(&repo, "f.txt");
        assert_eq!(ids.len(), remaining);
        restore(&repo, ids[..1].to_vec(), "f.txt");
        if remaining > 1 {
            assert_eq!(
                git(root, &["diff-files", "--name-only"]).trim(),
                "f.txt",
                "a file that keeps unstaged hunks stays stat-dirty"
            );
        }
    }

    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), original);
    assert_eq!(git(root, &["diff-files", "--name-only"]).trim(), "");
    assert_eq!(git(root, &["status", "--porcelain"]).trim(), "");
}

#[test]
fn partial_restore_keeps_crlf_worktree_line_endings() {
    let dir = init_repo();
    let root = dir.path();
    fs::write(root.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
    let lines: Vec<String> = (0..60).map(|i| format!("line{i}")).collect();
    fs::write(root.join("c.txt"), lines.join("\n") + "\n").unwrap();
    git(root, &["add", ".gitattributes", "c.txt"]);
    git(root, &["commit", "-qm", "base"]);
    fs::remove_file(root.join("c.txt")).unwrap();
    git(root, &["checkout", "--", "c.txt"]);
    assert!(fs::read_to_string(root.join("c.txt"))
        .unwrap()
        .starts_with("line0\r\n"));

    let mut edited = lines.clone();
    edited[5] = "LINE5".into();
    edited[50] = "LINE50".into();
    fs::write(root.join("c.txt"), edited.join("\r\n") + "\r\n").unwrap();

    let repo = open(root);
    let ids = unstaged_ids(&repo, "c.txt");
    assert_eq!(ids.len(), 2);
    restore(&repo, ids[..1].to_vec(), "c.txt");

    let mut expected = lines;
    expected[50] = "LINE50".into();
    assert_eq!(
        fs::read_to_string(root.join("c.txt")).unwrap(),
        expected.join("\r\n") + "\r\n"
    );
    assert_eq!(
        git(root, &["diff", "--numstat", "--", "c.txt"]).trim(),
        "1\t1\tc.txt"
    );
}
