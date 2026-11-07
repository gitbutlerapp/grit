//! Sparse-index collapse on write: grit folds an expanded index back into the same
//! sparse-directory rows Git writes, and Git reads the result.

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

use support::{assert_git_fsck_strict, git, HistoryRepo};

/// `(path, mode, oid, stage, skip-worktree)` for every row of the index file, unexpanded.
fn raw_rows(wt: &Path) -> Vec<(String, u32, String, u8, bool)> {
    Index::load(&wt.join(".git/index"))
        .expect("load index")
        .entries()
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                e.mode,
                e.oid.to_hex(),
                e.stage(),
                e.skip_worktree(),
            )
        })
        .collect()
}

/// A cone checkout of `a/` with excluded directories at several depths, written by Git as
/// a sparse index.
fn sparse_repo() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    for (path, body) in [
        ("a/in", "in\n"),
        ("top", "top\n"),
        ("w/f4", "f4\n"),
        ("w/f5", "f5\n"),
        ("x/f1", "f1\n"),
        ("x/y-z", "dash\n"),
        ("x/y/f2", "f2\n"),
        ("x/y/z/f3", "f3\n"),
    ] {
        let file = wt.join(path);
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        fs::write(file, body).expect("write");
    }
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-q", "-m", "base"]);
    for key in [
        "core.sparseCheckout",
        "core.sparseCheckoutCone",
        "index.sparse",
    ] {
        git(wt, &["config", key, "true"]);
    }
    fs::write(wt.join(".git/info/sparse-checkout"), "/*\n!/*/\n/a/\n").expect("patterns");
    git(wt, &["read-tree", "-mu", "HEAD"]);
    repo
}

#[test]
fn expanded_index_collapses_to_the_rows_git_writes() {
    if !support::skip_unless_git_at_least(2, 37) {
        return;
    }
    let repo = sparse_repo();
    let wt = repo.worktree();
    let git_rows = raw_rows(wt);
    assert!(
        git_rows.iter().any(|(path, ..)| path == "x/"),
        "Git wrote a sparse index: {git_rows:?}"
    );

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let mut index = grit
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default().with_path(&wt.join(".git/index")))
        .expect("expand");
    assert!(!index.has_sparse_directory_placeholders());
    grit.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())
        .expect("write");

    assert_eq!(raw_rows(wt), git_rows);
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert_git_fsck_strict(wt);
}

#[test]
fn changed_excluded_row_keeps_only_its_ancestors_expanded_for_git() {
    if !support::skip_unless_git_at_least(2, 37) {
        return;
    }
    let repo = sparse_repo();
    let wt = repo.worktree();
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let mut index = grit
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default().with_path(&wt.join(".git/index")))
        .expect("expand");
    let edited = grit
        .odb()
        .write(ObjectKind::Blob, b"edited\n")
        .expect("blob");
    let mut row = index
        .entries()
        .iter()
        .find(|e| e.path == b"x/y/f2")
        .expect("x/y/f2")
        .clone();
    row.oid = edited;
    index.add_or_replace(row);
    grit.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())
        .expect("write");

    let paths: Vec<String> = raw_rows(wt).into_iter().map(|(p, ..)| p).collect();
    assert_eq!(
        paths,
        ["a/in", "top", "w/", "x/f1", "x/y-z", "x/y/f2", "x/y/z/"]
    );
    assert_eq!(
        git(wt, &["diff", "--cached", "--name-only"]).trim(),
        "x/y/f2"
    );
    let staged = git(wt, &["ls-files", "-s", "x/y/f2"]);
    assert!(staged.contains(&edited.to_hex()), "{staged}");
}
