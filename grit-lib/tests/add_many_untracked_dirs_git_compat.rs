//! `stage` of a work tree with many untracked directories (issue #1854): the index grit
//! writes must match what system `git add` stages, with and without pathspecs and with a
//! normal-mode untracked cache on the index.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use std::fs;
use std::path::Path;

use support::{assert_git_fsck_strict, git};

fn open_grit(wt: &Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open grit repo")
}

/// Nested one-file directories, an ignored build tree and an embedded repository.
fn seed_tree(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
    for i in 0..300 {
        let dir = root.join(format!("top/d{}/s{}", i % 20, i % 7));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        fs::write(dir.join(format!("f{i}.log")), b"log\n").unwrap();
    }
    for i in 0..40 {
        let dir = root.join(format!("other/o{i}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("o.txt"), format!("o{i}\n")).unwrap();
    }
    fs::create_dir_all(root.join("top/d3/build/deep")).unwrap();
    fs::write(root.join("top/d3/build/deep/out.bin"), b"bin\n").unwrap();
    let inner = root.join("top/vendor/lib");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main", "."]);
    fs::write(inner.join("lib.txt"), b"lib\n").unwrap();
    git(&inner, &["add", "lib.txt"]);
    git(&inner, &["commit", "-q", "-m", "lib"]);
}

fn copy_tree(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp")
        .args(["-a"])
        .arg(from)
        .arg(to)
        .status()
        .expect("cp");
    assert!(status.success());
}

fn grit_stage(root: &Path, pathspecs: &[&str]) {
    let specs: Vec<String> = pathspecs.iter().map(|s| (*s).to_owned()).collect();
    stage(
        &open_grit(root),
        &StageOptions {
            pathspecs: specs.clone(),
            pathspec_sources: specs,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit stage");
}

/// Stage the same tree with grit and with `git add`, then compare `git ls-files -s`.
fn assert_stage_matches_git(prepare: impl Fn(&Path), pathspecs: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let grit_root = tmp.path().join("grit");
    let git_root = tmp.path().join("git");
    fs::create_dir_all(&grit_root).unwrap();
    seed_tree(&grit_root);
    prepare(&grit_root);
    copy_tree(&grit_root, &git_root);

    grit_stage(&grit_root, pathspecs);
    let mut add = vec!["add", "-A"];
    add.extend(pathspecs);
    git(&git_root, &add);

    let grit_listing = git(&grit_root, &["ls-files", "-s"]);
    let git_listing = git(&git_root, &["ls-files", "-s"]);
    assert_eq!(grit_listing, git_listing, "pathspecs {pathspecs:?}");
    let covers_vendor = pathspecs.is_empty() || pathspecs.iter().any(|s| *s == "." || *s == "top");
    assert_eq!(
        grit_listing.contains("160000 "),
        covers_vendor,
        "embedded repo gitlink for {pathspecs:?}"
    );
    assert!(!grit_listing.contains(".log"), "ignored files staged");
    assert!(!grit_listing.contains("build/"), "ignored directory staged");
    assert_git_fsck_strict(&grit_root);
}

#[test]
fn stage_dot_on_empty_index_matches_git_add() {
    assert_stage_matches_git(|_| {}, &["."]);
}

#[test]
fn stage_without_pathspec_on_tracked_index_matches_git_add() {
    assert_stage_matches_git(
        |root| {
            git(root, &["add", "other"]);
            git(root, &["commit", "-q", "-m", "other"]);
        },
        &[],
    );
}

#[test]
fn stage_pathspec_limited_matches_git_add() {
    for specs in [&["top"][..], &["top/d1", "other/o3"], &["*.txt"], &["."]] {
        assert_stage_matches_git(
            |root| {
                git(root, &["add", "other/o0"]);
                git(root, &["commit", "-q", "-m", "o0"]);
            },
            specs,
        );
    }
}

#[test]
fn stage_with_normal_mode_untracked_cache_matches_git_add() {
    assert_stage_matches_git(
        |root| {
            git(root, &["config", "core.untrackedCache", "true"]);
            git(root, &["add", "other/o0"]);
            git(root, &["commit", "-q", "-m", "o0"]);
            git(root, &["status", "--untracked-files=normal"]);
        },
        &[],
    );
}
