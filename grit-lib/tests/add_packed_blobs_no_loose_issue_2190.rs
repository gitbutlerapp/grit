//! Staging content that is already packed must not write it again as loose objects (#2190).
//!
//! Each scenario is set up with the system `git`, staged through
//! [`grit_lib::porcelain::add::stage`], and checked with `git count-objects`, `git fsck`
//! and `git write-tree`.

use std::fs;
use std::io::Write;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::stage_tracked::{
    stage_tracked_modifications_with_thresholds, IndexStagingThresholds,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::{git, unique_tmp};

const FILES: usize = 50;
/// Matches [`IndexStagingThresholds::parallel_tracked_restage`] and the #2328 repro scale.
const TRACKED_RESTAGE_FILES: usize = 5_000;

fn seed_packed_repo(name: &str) -> std::path::PathBuf {
    let root = unique_tmp("add-packed-2190", name);
    git(&root, &["init", "-q", "-b", "main", "."]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "gc.auto", "0"]);
    for i in 1..=FILES {
        fs::write(root.join(format!("f{i}.txt")), format!("file {i}\n")).expect("write");
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "one"]);
    git(&root, &["gc", "-q", "--prune=now"]);
    assert_eq!(loose_object_count(&root), 0);
    root
}

fn loose_object_count(repo: &Path) -> usize {
    git(repo, &["count-objects", "-v"])
        .lines()
        .find_map(|line| line.strip_prefix("count: ")?.trim().parse().ok())
        .expect("count-objects count line")
}

fn stage_with_grit(root: &Path, pathspecs: &[&str]) {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo");
    let opts = StageOptions {
        pathspecs: pathspecs.iter().map(|s| (*s).to_owned()).collect(),
        pathspec_sources: pathspecs.iter().map(|s| (*s).to_owned()).collect(),
        mode: StageMode::All,
        ..StageOptions::default()
    };
    stage(&repo, &opts, &mut NullProgress, &NeverCancel).expect("stage");
}

fn assert_index_matches_head_and_fsck(root: &Path) {
    let tree = git(root, &["write-tree"]);
    let head_tree = git(root, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(tree.trim(), head_tree.trim());
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn stage_all_after_rm_cached_writes_no_loose_objects() {
    let root = seed_packed_repo("all");
    git(&root, &["rm", "-q", "-r", "--cached", "."]);

    stage_with_grit(&root, &[]);

    assert_eq!(
        loose_object_count(&root),
        0,
        "re-adding packed content must not create loose objects"
    );
    assert_index_matches_head_and_fsck(&root);
}

#[test]
fn stage_one_path_after_rm_cached_writes_no_loose_object() {
    let root = seed_packed_repo("one-path");
    git(&root, &["rm", "-q", "--cached", "f3.txt"]);

    stage_with_grit(&root, &["f3.txt"]);

    assert_eq!(loose_object_count(&root), 0);
    assert_index_matches_head_and_fsck(&root);
}

#[test]
fn stage_glob_after_rm_cached_writes_no_loose_objects() {
    let root = seed_packed_repo("glob");
    git(&root, &["rm", "-q", "-r", "--cached", "."]);

    stage_with_grit(&root, &["f1*.txt"]);
    stage_with_grit(&root, &["*.txt"]);

    assert_eq!(loose_object_count(&root), 0);
    assert_index_matches_head_and_fsck(&root);
}

#[test]
fn stage_reverted_tracked_file_reuses_packed_blob() {
    let root = seed_packed_repo("revert");
    fs::write(root.join("f7.txt"), "changed\n").expect("write");
    git(&root, &["commit", "-qam", "two"]);
    git(&root, &["gc", "-q", "--prune=now"]);
    assert_eq!(loose_object_count(&root), 0);

    fs::write(root.join("f7.txt"), "file 7\n").expect("revert");
    stage_with_grit(&root, &[]);
    assert_eq!(loose_object_count(&root), 0);

    fs::write(root.join("f7.txt"), "changed\n").expect("re-apply");
    stage_with_grit(&root, &["*.txt"]);
    assert_eq!(loose_object_count(&root), 0);
    assert_index_matches_head_and_fsck(&root);
}

#[test]
fn stage_new_files_into_packed_repo_writes_them_loose() {
    let root = seed_packed_repo("new-files");
    git(&root, &["rm", "-q", "-r", "--cached", "."]);
    for i in 0..10 {
        fs::write(root.join(format!("new{i}.txt")), format!("new {i}\n")).expect("write");
    }

    stage_with_grit(&root, &[]);

    assert_eq!(
        loose_object_count(&root),
        10,
        "only the new content is written, as loose objects"
    );
    let grit_tree = git(&root, &["write-tree"]);
    git(&root, &["add", "-A"]);
    assert_eq!(grit_tree, git(&root, &["write-tree"]));
    git(&root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn tracked_restage_after_gc_matches_mod_branch_blobs_without_loose_writes() {
    let root = unique_tmp("add-packed-2190", "tracked-restage-5k");
    git(&root, &["init", "-q", "-b", "main", "."]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "gc.auto", "0"]);
    for i in 0..TRACKED_RESTAGE_FILES {
        let dir = root.join(format!("d{:02}", i % 50));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join(format!("f{i:04}.txt")),
            format!("file {}\n", i * 10),
        )
        .expect("write");
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    git(&root, &["switch", "-qc", "mod"]);
    for i in 0..TRACKED_RESTAGE_FILES {
        fs::OpenOptions::new()
            .append(true)
            .open(root.join(format!("d{:02}/f{i:04}.txt", i % 50)))
            .expect("open")
            .write_all(b"modified\n")
            .expect("append");
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "mod"]);
    git(&root, &["gc", "-q", "--prune=now"]);
    assert_eq!(loose_object_count(&root), 0);
    git(&root, &["switch", "-q", "main"]);
    for i in 0..TRACKED_RESTAGE_FILES {
        fs::OpenOptions::new()
            .append(true)
            .open(root.join(format!("d{:02}/f{i:04}.txt", i % 50)))
            .expect("open")
            .write_all(b"modified\n")
            .expect("append");
    }
    if let Ok(entries) = fs::read_dir(root.join(".git/objects/pack")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "pack") {
                let mut perms = fs::metadata(&path).expect("meta").permissions();
                perms.set_readonly(true);
                fs::set_permissions(&path, perms).expect("readonly pack like git gc on Windows");
            }
        }
    }
    let repo = Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("open repo");
    let thresholds = IndexStagingThresholds {
        parallel_tracked_restage: 4_096,
        ..IndexStagingThresholds::default()
    };
    let summary =
        stage_tracked_modifications_with_thresholds(&repo, &root, thresholds).expect("restage");
    assert_eq!(
        summary.modified.len(),
        TRACKED_RESTAGE_FILES,
        "every file should be restaged"
    );
    assert_eq!(
        loose_object_count(&root),
        0,
        "tracked restage of blobs already in pack must not write loose objects (#2328)"
    );
    git(&root, &["fsck", "--strict", "--no-dangling"]);
}

fn stage_after_rm_cached_in_shared_clone_writes_no_loose_objects() {
    let origin = seed_packed_repo("shared-origin");
    let clone = unique_tmp("add-packed-2190", "shared-clone");
    let parent = clone.parent().expect("parent");
    git(
        parent,
        &[
            "clone",
            "-q",
            "--shared",
            origin.to_str().expect("utf-8"),
            clone.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(loose_object_count(&clone), 0);
    git(&clone, &["rm", "-q", "-r", "--cached", "."]);

    stage_with_grit(&clone, &[]);

    assert_eq!(
        loose_object_count(&clone),
        0,
        "content stored in an alternate object directory is not copied locally"
    );
    assert_index_matches_head_and_fsck(&clone);
}
