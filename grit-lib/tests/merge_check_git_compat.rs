//! In-memory merge prediction and stored merge trees vs `git merge-tree --write-tree`.

use crate::support;

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::objects::{NewCommit, ObjectId};
use grit_lib::porcelain::merge::{
    predict_merge, write_merge_tree, MergeConflictKind, MergePrediction, MergePredictionOptions,
};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use time::OffsetDateTime;

use support::{
    assert_git_fsck_strict, git_merge_tree_oracle, git_merge_tree_oracle_with_options,
    git_version_at_least, GitUnmergedRow,
};
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

fn rev(root: &Path, spec: &str) -> ObjectId {
    git(root, &["rev-parse", spec])
        .trim()
        .parse()
        .expect("rev-parse oid")
}

fn git_object_type(root: &Path, oid: &ObjectId) -> Option<String> {
    let out = git_cmd(&["cat-file", "-t", &oid.to_hex()])
        .in_dir(root)
        .exec();
    out.ok().then(|| out.stdout.trim().to_owned())
}

fn signature() -> Signature {
    let when = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp");
    Signature::new("T", "t@example.com", when).expect("signature")
}

/// The #2113 reproduction: `ours` renames `dir` to `newdir`, `theirs` adds `dir/z`, so the
/// merged tree holds trees and entries that exist in neither side.
fn renamed_dir_repo() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "a@b"]);
    git(root, &["config", "user.name", "n"]);
    fs::create_dir(root.join("dir")).expect("mkdir dir");
    fs::write(root.join("dir/x"), "x1\nx2\nx3\nx4\n").expect("write x");
    fs::write(root.join("dir/y"), "y1\ny2\ny3\ny4\n").expect("write y");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "ours"]);
    git(root, &["mv", "dir", "newdir"]);
    git(root, &["commit", "-qm", "o"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["switch", "-qc", "theirs"]);
    fs::write(root.join("dir/z"), "z1\nz2\nz3\nz4\n").expect("write z");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "t"]);
    let ours = rev(root, "ours");
    let theirs = rev(root, "theirs");
    (tmp, ours, theirs)
}

fn no_ff() -> MergePredictionOptions {
    MergePredictionOptions {
        allow_fast_forward: false,
        ..MergePredictionOptions::default()
    }
}

#[test]
fn write_merge_tree_stores_renamed_directory_merge_for_commit() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, ours, theirs) = renamed_dir_repo();
    let root = tmp.path();
    let repo = grit_repo(root);

    let MergePrediction::Clean { tree: predicted } =
        predict_merge(&repo, ours, theirs, &no_ff()).expect("predict_merge")
    else {
        panic!("expected a clean prediction");
    };
    assert!(
        !repo.objects().contains(&predicted).expect("contains"),
        "predict_merge must not store the predicted tree"
    );

    let written = write_merge_tree(&repo, ours, theirs, &no_ff()).expect("write_merge_tree");
    assert!(written.is_clean(), "{written:?}");
    assert_eq!(
        written.tree, predicted,
        "stored tree must equal the prediction"
    );
    assert!(repo.objects().contains(&written.tree).expect("contains"));
    assert_eq!(
        git_object_type(root, &written.tree).as_deref(),
        Some("tree"),
        "git must read the stored tree before merge-tree writes its own copy"
    );

    let sig = signature();
    let commit = repo
        .objects()
        .write_commit(
            &NewCommit::new(written.tree, sig.clone(), sig, "m").with_parents(vec![ours, theirs]),
        )
        .expect("commit the stored merge tree");
    assert_eq!(git_object_type(root, &commit).as_deref(), Some("commit"));
    assert_git_fsck_strict(root);
}

/// Both sides edit different lines of `sub/f`, so the clean merge needs a new blob, a new
/// `sub` tree and a new root tree that exist on neither side.
fn content_merge_repo() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::create_dir(root.join("sub")).expect("mkdir sub");
    fs::write(root.join("sub/f"), "1\n2\n3\n4\n5\n6\n").expect("write f");
    fs::write(root.join("top"), "top\n").expect("write top");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "ours"]);
    fs::write(root.join("sub/f"), "one\n2\n3\n4\n5\n6\n").expect("edit ours");
    git(root, &["commit", "-qam", "ours"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["switch", "-qc", "theirs"]);
    fs::write(root.join("sub/f"), "1\n2\n3\n4\n5\nsix\n").expect("edit theirs");
    git(root, &["commit", "-qam", "theirs"]);
    let ours = rev(root, "ours");
    let theirs = rev(root, "theirs");
    (tmp, ours, theirs)
}

#[test]
fn write_merge_tree_works_in_bare_repository() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, ours, theirs) = content_merge_repo();
    let bare = tmp.path().join("bare.git");
    git(
        tmp.path(),
        &["clone", "-q", "--bare", ".", &bare.to_string_lossy()],
    );
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open bare");
    let loose_before = count_loose_objects(&bare);

    let written = write_merge_tree(&repo, ours, theirs, &no_ff()).expect("write_merge_tree");
    assert!(written.is_clean());
    assert!(
        count_loose_objects(&bare) > loose_before,
        "new merge objects must be written"
    );
    assert_eq!(
        git_object_type(&bare, &written.tree).as_deref(),
        Some("tree")
    );
    assert_eq!(
        git(
            &bare,
            &["cat-file", "-p", &format!("{}:sub/f", written.tree)]
        ),
        "one\n2\n3\n4\n5\nsix\n"
    );
    let sig = signature();
    let commit = repo
        .objects()
        .write_commit(
            &NewCommit::new(written.tree, sig.clone(), sig, "m").with_parents(vec![ours, theirs]),
        )
        .expect("commit the stored merge tree");
    assert_eq!(git_object_type(&bare, &commit).as_deref(), Some("commit"));
    assert_git_fsck_strict(&bare);

    let oracle = git_merge_tree_oracle(&bare, None, &ours, &theirs, MergeFavor::None);
    assert_eq!(
        written.tree, oracle.result_tree,
        "tree must match git merge-tree"
    );
    assert!(oracle.unmerged.is_empty());
}

#[test]
fn write_merge_tree_conflict_stages_match_git_merge_tree() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let (tmp, _base, _main, topic) = sibling_merge_repo();
    let root = tmp.path();
    fs::write(root.join("a.txt"), "main conflict\n").expect("conflict main");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "main conflict"]);
    let main = rev(root, "HEAD");
    let repo = grit_repo(root);

    // `git merge-tree <oid> <oid>` labels the conflict markers with the arguments it was given.
    let options = MergePredictionOptions {
        ours_label: main.to_hex(),
        theirs_label: topic.to_hex(),
        ..MergePredictionOptions::default()
    };
    let written = write_merge_tree(&repo, main, topic, &options).expect("write_merge_tree");
    assert!(!written.is_clean());
    assert_eq!(written.conflicts.len(), 1);
    assert_eq!(written.conflicts[0].path, "a.txt");
    assert_eq!(written.conflicts[0].kind, MergeConflictKind::Content);
    assert_eq!(
        git_object_type(root, &written.tree).as_deref(),
        Some("tree")
    );
    let merged_blob = git(
        root,
        &["cat-file", "-p", &format!("{}:a.txt", written.tree)],
    );
    assert!(merged_blob.contains("<<<<<<< "), "{merged_blob}");

    let oracle = git_merge_tree_oracle(root, None, &main, &topic, MergeFavor::None);
    assert_eq!(
        written.tree, oracle.result_tree,
        "tree must match git merge-tree"
    );
    let rows: Vec<GitUnmergedRow> = written
        .stages
        .iter()
        .map(|s| GitUnmergedRow {
            mode: s.mode,
            oid: s.oid,
            stage: u32::from(s.stage),
            path: String::from_utf8(s.path.clone()).expect("utf-8 path"),
        })
        .collect();
    assert_eq!(
        rows, oracle.unmerged,
        "stage rows must match git merge-tree"
    );
    assert_git_fsck_strict(root);
}

#[test]
fn write_merge_tree_unrelated_histories() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "root"]);
    let main = rev(root, "HEAD");
    git(root, &["switch", "-q", "--orphan", "orphan"]);
    fs::write(root.join("b.txt"), "b\n").expect("write b");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-qm", "orphan"]);
    let orphan = rev(root, "HEAD");
    let repo = grit_repo(root);

    let err = write_merge_tree(&repo, main, orphan, &MergePredictionOptions::default())
        .expect_err("unrelated histories must be refused by default");
    assert!(
        matches!(err, Error::UnrelatedHistories { ours, theirs } if ours == main && theirs == orphan),
        "{err:?}"
    );

    let options = MergePredictionOptions {
        allow_unrelated_histories: true,
        ..MergePredictionOptions::default()
    };
    let written = write_merge_tree(&repo, main, orphan, &options).expect("write_merge_tree");
    assert!(written.is_clean());
    assert_eq!(
        git_object_type(root, &written.tree).as_deref(),
        Some("tree")
    );
    let oracle =
        git_merge_tree_oracle_with_options(root, None, &main, &orphan, MergeFavor::None, true);
    assert_eq!(written.tree, oracle.result_tree);
}

#[test]
fn write_merge_tree_fast_forwardable_merge_yields_tip_tree() {
    let (tmp, base, main, _topic) = sibling_merge_repo();
    let repo = grit_repo(tmp.path());
    let written = write_merge_tree(&repo, base, main, &MergePredictionOptions::default())
        .expect("write_merge_tree");
    assert!(written.is_clean());
    assert_eq!(
        written.tree,
        repo.objects().commit_tree(&main).expect("tree")
    );
    let written = write_merge_tree(&repo, main, base, &MergePredictionOptions::default())
        .expect("write_merge_tree");
    assert_eq!(
        written.tree,
        repo.objects().commit_tree(&main).expect("tree")
    );
}
