use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{commit_diff_files, DiffDetail, DiffMerges, DiffOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn empty_parent_merge_with_blob(dir: &std::path::Path, blob: &[u8], name: &str) -> ObjectId {
    let empty: ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".parse().unwrap();
    std::fs::write(dir.join(name), blob).unwrap();
    git(dir, &["add", name]);
    let tree = git(dir, &["write-tree"])
        .trim()
        .parse::<ObjectId>()
        .unwrap();
    let p1: ObjectId = git(dir, &["commit-tree", &empty.to_hex(), "-m", "p1"])
        .trim()
        .parse()
        .unwrap();
    let p2: ObjectId = git(dir, &["commit-tree", &empty.to_hex(), "-m", "p2"])
        .trim()
        .parse()
        .unwrap();
    git(
        dir,
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
    .unwrap()
}

#[test]
fn combined_merge_show_includes_added_file() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    let merge = empty_parent_merge_with_blob(dir.path(), b"hi\n", "added.txt");
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

#[test]
fn combined_merge_binary_addition_is_binary_not_text() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    let merge =
        empty_parent_merge_with_blob(dir.path(), &[0x00, 0x61, 0x62, 0x63, 0xff], "binary.dat");
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
    let file = out
        .files
        .iter()
        .find(|f| f.path == "binary.dat")
        .expect("binary.dat in combined diff");
    assert!(
        file.binary,
        "expected binary flag, combined={:?}",
        file.combined
    );
    assert!(
        file.combined.is_none(),
        "must not build structured text hunks"
    );
}
