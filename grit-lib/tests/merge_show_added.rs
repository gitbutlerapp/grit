use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{commit_diff_files, DiffDetail, DiffMerges, DiffOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;

#[test]
fn combined_merge_show_includes_added_file() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    let empty: ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".parse().unwrap();
    std::fs::write(dir.path().join("added.txt"), b"hi\n").unwrap();
    git(dir.path(), &["add", "added.txt"]);
    let tree = git(dir.path(), &["write-tree"])
        .trim()
        .parse::<ObjectId>()
        .unwrap();
    let p1: ObjectId = git(dir.path(), &["commit-tree", &empty.to_hex(), "-m", "p1"])
        .trim()
        .parse()
        .unwrap();
    let p2: ObjectId = git(dir.path(), &["commit-tree", &empty.to_hex(), "-m", "p2"])
        .trim()
        .parse()
        .unwrap();
    let merge: ObjectId = git(
        dir.path(),
        &[
            "commit-tree",
            &tree.to_hex(),
            "-p",
            &p1.to_hex(),
            "-p",
            &p2.to_hex(),
            "-m",
            "merge",
        ],
    )
    .trim()
    .parse()
    .unwrap();
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .unwrap();
    let out = commit_diff_files(
        &repo,
        &merge,
        DiffMerges::Combined,
        DiffDetail::Patch,
        &DiffOptions::default(),
        &[],
        repo.config().unwrap().as_ref(),
        &repo.thread_policy(),
    )
    .unwrap();
    assert!(
        !out.files.is_empty(),
        "expected combined diff files, got {:?}",
        out.files
    );
    assert!(out.files.iter().any(|f| f.path == "added.txt"));
}
