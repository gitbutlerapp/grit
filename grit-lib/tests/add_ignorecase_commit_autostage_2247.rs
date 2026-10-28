//! Issue #2247: auto-stage must not respell tracked paths on case-folding volumes, and must
//! fold new untracked paths under existing index directory spellings.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, stage_for_commit, StageMode, StageOptions};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn index_paths(root: &Path) -> Vec<String> {
    let index = Index::load(&root.join(".git/index")).expect("index");
    let mut paths: Vec<String> = index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths
}

fn is_case_insensitive(dir: &Path) -> bool {
    for attempt in 0..128_u32 {
        let name = format!(".grit-icase-test-probe-{attempt:05}");
        let lower = dir.join(&name);
        if lower.exists() {
            continue;
        }
        if std::fs::write(&lower, b"").is_err() {
            return false;
        }
        let upper: String = name.chars().map(|c| c.to_ascii_uppercase()).collect();
        let folded = dir.join(upper).exists();
        let _ = std::fs::remove_file(&lower);
        return folded;
    }
    false
}

/// Repro A on Linux: both `Src/` and `src/` exist; unrelated commit must not rewrite either.
#[test]
fn ignorecase_unrelated_commit_preserves_dual_case_dirs_on_sensitive_volume() {
    if is_case_insensitive(std::env::temp_dir().as_path()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("Src")).expect("Src");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("Src/a.txt"), "a\n").expect("a");
    fs::write(root.join("src/b.txt"), "b\n").expect("b");
    for n in ["Src/a.txt", "src/b.txt"] {
        let oid = git(root, &["hash-object", "-w", n]).trim().to_string();
        git(
            root,
            &["update-index", "--add", "--cacheinfo", "100644", &oid, n],
        );
    }
    git(root, &["commit", "-qm", "coll"]);
    let before = index_paths(root);

    fs::write(root.join("unrelated.txt"), "edit\n").expect("unrelated");
    let repo = open(root);
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
    commit_worktree(
        &repo,
        &CommitRequest {
            message: "unrelated".to_owned(),
            ..CommitRequest::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");

    let after = index_paths(root);
    for path in &before {
        assert!(
            after.contains(path),
            "index path {path} must be unchanged, got {after:?}"
        );
    }
    let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let mut tree_paths: Vec<_> = tree.lines().collect();
    tree_paths.sort();
    assert!(tree_paths.contains(&"Src/a.txt"));
    assert!(tree_paths.contains(&"src/b.txt"));
    assert!(tree_paths.contains(&"unrelated.txt"));
}

/// `memihash` directory collision must not fold unrelated paths (reviewer repro).
#[test]
fn ignorecase_add_stages_hash_collision_dirs_independently() {
    if is_case_insensitive(std::env::temp_dir().as_path()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("f02398b.txt")).expect("dir1");
    fs::write(root.join("f02398b.txt/x"), "one\n").expect("x");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    fs::create_dir_all(root.join("f0688b8.txt")).expect("dir2");
    fs::write(root.join("f0688b8.txt/y"), "two\n").expect("y");

    let repo = open(root);
    stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage must not ENOENT on hash-collision directory names");

    let mut paths = index_paths(root);
    paths.sort();
    assert_eq!(
        paths,
        vec!["f02398b.txt/x".to_owned(), "f0688b8.txt/y".to_owned()]
    );
}

/// Pre-existing uppercase probe decoy must not disable case-sensitive respell (reviewer regression).
#[test]
fn ignorecase_sensitive_volume_respells_after_uppercase_probe_decoy() {
    if is_case_insensitive(std::env::temp_dir().as_path()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join(".gitignore"), ".GRIT-ICASE-FS-PROBE-00000\n").expect("gitignore");
    fs::write(root.join(".GRIT-ICASE-FS-PROBE-00000"), b"decoy\n").expect("decoy");

    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("Dir")).expect("Dir");
    fs::write(root.join("Dir/f.txt"), "b\n").expect("f");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    fs::rename(root.join("Dir"), root.join("dir")).expect("rename dir");

    let repo = open(root);
    stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

    let paths = index_paths(root);
    assert!(
        paths.iter().any(|p| p == "dir/f.txt"),
        "case-sensitive volume must respell tracked paths, got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p == "Dir/f.txt"),
        "old spelling must not remain after respell: {paths:?}"
    );
}

/// Case-sensitive volume: new `dir/g.txt` folds to indexed `Dir/` without ENOENT (issue #2247).
#[test]
fn ignorecase_add_folds_new_file_under_index_dir_on_sensitive_volume() {
    if is_case_insensitive(std::env::temp_dir().as_path()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("Dir")).expect("Dir");
    fs::write(root.join("Dir/f.txt"), "b\n").expect("f");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    fs::rename(root.join("Dir"), root.join("dir")).expect("rename dir");
    fs::write(root.join("dir/g.txt"), "g\n").expect("g");

    let repo = open(root);
    stage(
        &repo,
        &StageOptions {
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

    let paths = index_paths(root);
    assert!(
        paths.iter().any(|p| p == "Dir/g.txt"),
        "new file must stage under indexed directory spelling, got {paths:?}"
    );
}

/// Repro B on a case-folding volume: new files use index directory spelling; leaf aliases update in place.
#[test]
fn ignorecase_commit_keeps_index_spelling_on_folding_volume() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    if !is_case_insensitive(root) {
        return;
    }
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("Dir")).expect("Dir");
    fs::write(root.join("Dir/f.txt"), "b\n").expect("f");
    fs::write(root.join("readme.txt"), "r\n").expect("readme");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    fs::rename(root.join("Dir"), root.join("dir")).expect("rename dir");
    fs::rename(root.join("readme.txt"), root.join("README.txt")).expect("rename readme");
    fs::write(root.join("dir/g.txt"), "g\n").expect("g");
    fs::write(root.join("README.txt"), "n\n").expect("readme edit");

    let repo = open(root);
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
    commit_worktree(
        &repo,
        &CommitRequest {
            message: "two".to_owned(),
            ..CommitRequest::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");

    let expected = ["Dir/f.txt", "Dir/g.txt", "readme.txt"];
    let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let mut tree_paths: Vec<_> = tree.lines().collect();
    tree_paths.sort();
    assert_eq!(tree_paths, expected);
    git(root, &["fsck", "--strict"]);
}
