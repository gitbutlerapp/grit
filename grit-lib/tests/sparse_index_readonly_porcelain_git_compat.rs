//! Read-only porcelain on Git-written cone sparse-index repos must not expand the index.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{Diff, DiffStatus, RenameDetection};
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_at_least};

fn sparse_directory_placeholder_count(index: &Index) -> usize {
    index
        .entries()
        .iter()
        .filter(|e| e.is_sparse_directory_placeholder())
        .count()
}

fn git_sparse_entries(wt: &std::path::Path) -> usize {
    git(wt, &["ls-files", "--sparse"])
        .lines()
        .filter(|l| !l.is_empty())
        .count()
}

fn git_index_sparse_flag(wt: &std::path::Path) -> bool {
    git(wt, &["config", "--get", "index.sparse"]).trim() == "true"
}

/// Cone sparse-index repo shaped like issue #2335: deep directories outside the cone.
fn nested_sparse_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path();
    git(wt, &["init", "-q", "-b", "main", "."]);
    git(wt, &["config", "user.email", "t@example.com"]);
    git(wt, &["config", "user.name", "Test"]);

    for a in 0..3 {
        for b in 0..2 {
            for c in 0..2 {
                let d = format!("svc{:02}/mod{:02}/pkg{}", a, b, c);
                fs::create_dir_all(wt.join(&d)).expect("mkdir");
                for f in 0..5 {
                    fs::write(
                        wt.join(&d).join(format!("file{f}.txt")),
                        format!("{d} {f}\n"),
                    )
                    .expect("write");
                }
            }
        }
        fs::write(wt.join(format!("svc{:02}/top.txt", a)), "top\n").expect("top");
    }
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-qm", "init"]);
    git(
        wt,
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--sparse-index",
            "svc00/mod00",
            "svc01",
        ],
    );
    assert!(git_index_sparse_flag(wt), "fixture must use sparse index");
    tmp
}

#[test]
fn readonly_porcelain_keeps_git_sparse_index_shape() {
    if !git_at_least(2, 37) {
        return;
    }
    let tmp = nested_sparse_repo();
    let wt = tmp.path();
    let entries_before = git_sparse_entries(wt);
    let placeholders_before =
        sparse_directory_placeholder_count(&Index::load(&wt.join(".git/index")).expect("load"));
    assert!(
        placeholders_before > 0,
        "Git must write sparse-directory placeholders"
    );

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");

    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.is_empty() && model.staged.is_empty(),
        "clean tree should report no changes"
    );
    assert_eq!(
        git_sparse_entries(wt),
        entries_before,
        "status must not change sparse index entry count"
    );

    let diff = Diff::new(&grit_repo);
    let uncommitted = diff.uncommitted().expect("uncommitted diff");
    assert!(uncommitted
        .staged_entries(RenameDetection::Off)
        .expect("staged")
        .is_empty());
    assert_eq!(
        git_sparse_entries(wt),
        entries_before,
        "diff must not change sparse index entry count"
    );

    stage(
        &grit_repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("add");
    assert_eq!(
        git_sparse_entries(wt),
        entries_before,
        "add on clean tree must not change sparse index entry count"
    );

    let index_after = Index::load(&wt.join(".git/index")).expect("reload index");
    assert_eq!(
        sparse_directory_placeholder_count(&index_after),
        placeholders_before,
        "sparse-directory placeholder count must stay unchanged"
    );
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert!(Command::new("git")
        .args(["fsck"])
        .current_dir(wt)
        .status()
        .expect("fsck")
        .success());
}

#[test]
fn staged_out_of_cone_modification_with_mismatched_sparse_placeholder() {
    if !git_at_least(2, 37) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path();
    git(wt, &["init", "-q", "-b", "main", "."]);
    git(wt, &["config", "user.email", "t@example.com"]);
    git(wt, &["config", "user.name", "Test"]);
    fs::create_dir_all(wt.join("out/deep")).expect("mkdir out");
    fs::write(wt.join("out/deep/f"), "old\n").expect("write out");
    fs::create_dir_all(wt.join("in")).expect("mkdir in");
    fs::write(wt.join("in/f"), "in\n").expect("write in");
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-qm", "base"]);
    git(wt, &["sparse-checkout", "init", "--cone", "--sparse-index"]);
    git(wt, &["sparse-checkout", "set", "in"]);
    fs::create_dir_all(wt.join("out/deep")).expect("materialize out/deep");
    fs::write(wt.join("out/deep/f"), "new\n").expect("stage new content");
    git(wt, &["add", "--sparse", "out/deep/f"]);
    git(wt, &["sparse-checkout", "reapply", "--sparse-index"]);

    assert_eq!(
        git(wt, &["diff", "--cached", "--name-status"]).trim(),
        "M\tout/deep/f"
    );
    assert!(
        git(wt, &["ls-files", "-s", "--sparse"])
            .lines()
            .any(|line| line.ends_with("\tout/") || line.ends_with(" out/")),
        "Git must keep a sparse-directory row for out/"
    );

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let staged = Diff::new(&grit_repo)
        .uncommitted()
        .expect("load index")
        .staged_entries(RenameDetection::Off)
        .expect("staged diff");
    assert_eq!(staged.len(), 1, "expected one staged change: {staged:?}");
    assert_eq!(staged[0].path(), "out/deep/f");
    assert_eq!(staged[0].status, DiffStatus::Modified);
}

#[test]
fn dirty_in_cone_file_still_reports_without_expanding_outside_cone() {
    if !git_at_least(2, 37) {
        return;
    }
    let tmp = nested_sparse_repo();
    let wt = tmp.path();
    let dirty = wt.join("svc00/mod00/pkg0/file0.txt");
    fs::write(&dirty, "edited\n").expect("edit in cone");

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let placeholders_before =
        sparse_directory_placeholder_count(&Index::load(&wt.join(".git/index")).expect("load"));

    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert_eq!(model.unstaged.len(), 1);
    assert_eq!(model.unstaged[0].path(), "svc00/mod00/pkg0/file0.txt");

    let index_after = Index::load(&wt.join(".git/index")).expect("reload");
    assert_eq!(
        sparse_directory_placeholder_count(&index_after),
        placeholders_before
    );
}
