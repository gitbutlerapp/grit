//! In-memory merge prediction vs `git merge-tree --write-tree`.

mod support;

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{predict_merge, MergePrediction, MergePredictionOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

use support::{git_merge_tree_oracle, git_version_at_least};
fn grit_repo(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open grit repo")
}

fn count_loose_objects(git_dir: &Path) -> usize {
    let objects = git_dir.join("objects");
    let mut count = 0usize;
    if let Ok(entries) = fs::read_dir(&objects) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() != 2 || name == "pack" || name == "info" {
                continue;
            }
            if let Ok(sub) = fs::read_dir(entry.path()) {
                count += sub.flatten().count();
            }
        }
    }
    count
}

fn index_mtime(git_dir: &Path) -> SystemTime {
    fs::metadata(git_dir.join("index"))
        .expect("index metadata")
        .modified()
        .expect("index mtime")
}

fn sibling_merge_repo() -> (tempfile::TempDir, ObjectId, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    std::fs::write(root.join("a.txt"), "base\n").expect("write a");
    std::fs::write(root.join("b.txt"), "b\n").expect("write b");
    git(root, &["add", "a.txt", "b.txt"]);
    git(root, &["commit", "-m", "base"]);
    let base: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("base");

    git(root, &["switch", "-c", "topic"]);
    std::fs::write(root.join("a.txt"), "topic\n").expect("write topic");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "topic"]);
    let topic: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("topic");

    git(root, &["switch", "main"]);
    std::fs::write(root.join("b.txt"), "main\n").expect("write main");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-m", "main"]);
    let main: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    (tmp, base, main, topic)
}

#[test]
fn predict_clean_matches_git_merge_tree_tree_oid() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, base, main, topic) = sibling_merge_repo();
    let repo = grit_repo(tmp.path());

    let oracle = git_merge_tree_oracle(tmp.path(), Some(&base), &main, &topic, MergeFavor::None);
    let prediction = predict_merge(&repo, main, topic, &MergePredictionOptions::default())
        .expect("predict_merge");
    match prediction {
        MergePrediction::Clean { tree } => {
            assert_eq!(
                tree, oracle.result_tree,
                "predicted tree must match git merge-tree"
            );
        }
        other => panic!("expected clean prediction, got {other:?}"),
    }
}

#[test]
fn predict_conflicts_match_git_merge_tree_paths() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, base, _main, topic) = sibling_merge_repo();
    std::fs::write(tmp.path().join("a.txt"), "main conflict\n").expect("conflict main");
    git(tmp.path(), &["add", "a.txt"]);
    git(tmp.path(), &["commit", "-m", "main conflict"]);
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    let repo = grit_repo(tmp.path());
    let oracle = git_merge_tree_oracle(tmp.path(), Some(&base), &main, &topic, MergeFavor::None);
    assert!(
        !oracle.unmerged.is_empty(),
        "oracle must report conflicts for this setup"
    );

    let prediction = predict_merge(&repo, main, topic, &MergePredictionOptions::default())
        .expect("predict_merge");
    let MergePrediction::Conflicts { conflicts } = prediction else {
        panic!("expected conflicts prediction");
    };
    let grit_paths: Vec<_> = conflicts.iter().map(|c| c.path.as_str()).collect();
    let git_paths: Vec<_> = oracle
        .unmerged
        .iter()
        .map(|r| r.path.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        grit_paths, git_paths,
        "conflicted paths must match git merge-tree"
    );
}

#[test]
fn predict_writes_no_objects() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, _base, main, topic) = sibling_merge_repo();
    let repo = grit_repo(tmp.path());
    let git_dir = repo.git_dir();
    let loose_before = count_loose_objects(git_dir);
    let index_before = index_mtime(git_dir);

    predict_merge(&repo, main, topic, &MergePredictionOptions::default()).expect("predict");

    assert_eq!(
        count_loose_objects(git_dir),
        loose_before,
        "loose object count must be unchanged"
    );
    assert_eq!(
        index_mtime(git_dir),
        index_before,
        "index mtime must be unchanged"
    );
}

#[test]
fn predict_on_dirty_worktree_ok() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, _base, main, topic) = sibling_merge_repo();
    std::fs::write(tmp.path().join("b.txt"), "dirty unrelated\n").expect("dirty");
    let repo = grit_repo(tmp.path());
    let prediction = predict_merge(&repo, main, topic, &MergePredictionOptions::default())
        .expect("predict on dirty worktree");
    assert!(matches!(prediction, MergePrediction::Clean { .. }));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("b.txt")).expect("read b"),
        "dirty unrelated\n"
    );
}

#[test]
fn predict_unrelated_histories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "root"]);
    let main: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    git(root, &["switch", "--orphan", "orphan"]);
    std::fs::write(root.join("b.txt"), "b\n").expect("write b");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-m", "orphan"]);
    let orphan: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("orphan");

    let repo = grit_repo(root);
    let prediction = predict_merge(&repo, main, orphan, &MergePredictionOptions::default())
        .expect("predict unrelated");
    assert_eq!(prediction, MergePrediction::Unrelated);
}
