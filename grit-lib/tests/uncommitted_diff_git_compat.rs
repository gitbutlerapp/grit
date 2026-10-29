//! `grit_lib::diff::UncommittedDiff` and `load_object_changes` against system `git diff`:
//! staged, unstaged and working-tree entries match `--name-status`, and the loaded sides'
//! line counts match `--numstat`.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    load_object_changes, name_status_line, resolve_diff_request, DiffOptions, DiffRequest,
    DiffTarget, FileChange, RenameDetection, ResolveDiffArgumentsError, UncommittedDiff,
};
use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::{git, rev_parse};

const RENAMES: RenameDetection = RenameDetection::Similarity(50);

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

/// Committed files, then staged edits (one rename) plus further unstaged edits on top.
fn fixture(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    write(root, "keep.txt", "a\nb\nc\n");
    write(root, "both.txt", "1\n2\n3\n4\n");
    write(root, "gone.txt", "bye\n");
    write(root, "dir/move-me.txt", &long);
    write(root, "worktree-only.txt", "x\ny\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);

    write(root, "both.txt", "1\nTWO\n3\n4\n");
    fs::remove_file(root.join("gone.txt")).expect("rm");
    write(root, "added.txt", "new\nfile\n");
    git(root, &["mv", "dir/move-me.txt", "dir/moved.txt"]);
    git(root, &["add", "-A"]);

    write(root, "both.txt", "1\nTWO\n3\nFOUR\nfive\n");
    write(root, "worktree-only.txt", "x\n");
    write(root, "added.txt", "new\n");
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn names(entries: &[grit_lib::diff::DiffEntry]) -> Vec<String> {
    entries.iter().map(name_status_line).collect()
}

fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    git(root, args).lines().map(str::to_owned).collect()
}

fn numstat(changes: &[FileChange]) -> Vec<String> {
    changes
        .iter()
        .map(|c| {
            let stat = c.line_stat(&DiffOptions::default()).expect("text change");
            let path = match &c.old_path {
                Some(old) => format!("{old} => {}", c.path),
                None => c.path.clone(),
            };
            format!("{}\t{}\t{path}", stat.insertions, stat.deletions)
        })
        .collect()
}

/// `git diff --numstat` with renames, spelling a rename as `old => new` like [`numstat`].
fn git_numstat(root: &Path, args: &[&str]) -> Vec<String> {
    let mut full = vec!["diff", "--numstat", "-M"];
    full.extend_from_slice(args);
    git_lines(root, &full)
        .into_iter()
        .map(|line| {
            line.replace(
                "dir/{move-me.txt => moved.txt}",
                "dir/move-me.txt => dir/moved.txt",
            )
        })
        .collect()
}

#[test]
fn staged_entries_and_contents_match_git_diff_cached() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let repo = open(dir.path());
    let diff = repo.diff().uncommitted().expect("uncommitted");

    let entries = diff.staged_entries(RENAMES).expect("staged");
    assert_eq!(
        names(&entries),
        git_lines(dir.path(), &["diff", "--cached", "--name-status", "-M"])
    );

    let changes = load_object_changes(&repo, &entries).expect("load");
    assert_eq!(numstat(&changes), git_numstat(dir.path(), &["--cached"]));
    let both = changes.iter().find(|c| c.path == "both.txt").expect("both");
    assert_eq!(both.new_bytes.data, b"1\nTWO\n3\n4\n");
    assert_eq!(both.new_oid, rev_parse(dir.path(), ":both.txt"));
    let moved = changes
        .iter()
        .find(|c| c.path == "dir/moved.txt")
        .expect("rename");
    assert_eq!(moved.old_path.as_deref(), Some("dir/move-me.txt"));
    assert_eq!(moved.similarity, Some(100));
    assert!(!moved.content_changed);
}

#[test]
fn unstaged_entries_and_worktree_contents_match_git_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let repo = open(dir.path());
    let mut diff = repo.diff().uncommitted().expect("uncommitted");

    let entries = diff.unstaged_entries(RENAMES).expect("unstaged");
    assert_eq!(
        names(&entries),
        git_lines(dir.path(), &["diff", "--name-status", "-M"])
    );

    let changes = diff.load_worktree_changes(entries).expect("load");
    assert_eq!(numstat(&changes), git_numstat(dir.path(), &[]));
    for change in &changes {
        let on_disk = fs::read(dir.path().join(&change.path)).expect("read");
        assert_eq!(change.new_bytes.data, on_disk, "{}", change.path);
        let hunks = change.structured(DiffOptions::default()).hunks;
        assert!(!hunks.is_empty(), "{}", change.path);
    }
}

#[test]
fn worktree_entries_match_git_diff_against_a_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let repo = open(dir.path());
    let head_tree = rev_parse(dir.path(), "HEAD^{tree}");

    let mut diff = repo.diff().uncommitted().expect("uncommitted");
    let entries = diff
        .worktree_entries(Some(&head_tree), RENAMES)
        .expect("worktree vs HEAD");
    assert_eq!(
        names(&entries),
        git_lines(dir.path(), &["diff", "HEAD", "--name-status", "-M"])
    );
    let changes = diff.load_worktree_changes(entries).expect("load");
    assert_eq!(numstat(&changes), git_numstat(dir.path(), &["HEAD"]));

    // Empty base tree (as for an unborn HEAD): every tracked path is added.
    let mut diff = repo.diff().uncommitted().expect("uncommitted");
    let entries = diff
        .worktree_entries(None, RenameDetection::Off)
        .expect("worktree vs empty tree");
    let empty_tree = git(dir.path(), &["hash-object", "-t", "tree", "/dev/null"]);
    assert_eq!(
        names(&entries),
        git_lines(
            dir.path(),
            &["diff", empty_tree.trim(), "--name-status", "--no-renames"]
        )
    );
}

#[test]
fn index_accessor_reuses_the_loaded_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let repo = open(dir.path());
    let index = repo.index_handle().load().expect("index");
    let count = index.entries().len();
    let mut diff = UncommittedDiff::new(&repo, index);
    diff.unstaged_entries(RenameDetection::Off)
        .expect("unstaged");
    assert_eq!(diff.index().entries().len(), count);
    assert!(diff.staged_entries(RenameDetection::Off).is_ok());
}

#[test]
fn bare_repository_rejects_working_tree_diffs() {
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let bare = dir.path().join("bare.git");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.to_str().expect("utf-8 path"),
        ],
    );
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open bare");

    for request in [
        DiffRequest::default(),
        DiffRequest {
            unstaged: true,
            ..DiffRequest::default()
        },
        DiffRequest {
            against: Some("HEAD".to_owned()),
            ..DiffRequest::default()
        },
    ] {
        assert!(matches!(
            resolve_diff_request(&repo, None, &request),
            Err(ResolveDiffArgumentsError::NoWorkTree)
        ));
    }
    let staged = DiffRequest {
        staged: true,
        ..DiffRequest::default()
    };
    let arguments = resolve_diff_request(&repo, None, &staged).expect("staged works bare");
    assert_eq!(arguments.target, DiffTarget::IndexVsHead);

    let mut diff = UncommittedDiff::new(&repo, Index::default());
    assert!(matches!(
        diff.worktree_entries(Some(&ObjectId::zero()), RenameDetection::Off),
        Err(Error::RequiresWorkTree)
    ));
}
