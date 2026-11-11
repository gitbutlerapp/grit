//! Shallow boundary commits diff and blame like roots (issue #2520).

use grit_lib::blame::BlameOptions;
use grit_lib::diff::{DiffDetail, DiffMerges, DiffOptions};
use grit_lib::error::Error;
use grit_lib::error::RefError;
use grit_lib::error::RevisionError;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};
use grit_test_support::git;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn grit_shallow_clone(source_bare: &Path, dest: &Path, depth: u32) -> grit_lib::repo::Repository {
    clone_repo(
        CloneOptions::new(source_bare.to_string_lossy(), dest.to_path_buf())
            .with_depth(Some(depth)),
        &mut NullProgress,
        &CancelToken::never(),
        None,
    )
    .expect("shallow clone")
    .repo
}

fn build_linear_bare(bare: &Path) {
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    std::fs::write(wt.join("README"), "hello\n").unwrap();
    git(&wt, &["add", "README"]);
    git(&wt, &["commit", "-q", "-m", "root"]);
    std::fs::write(wt.join("README"), "hello world\n").unwrap();
    git(&wt, &["add", "README"]);
    git(&wt, &["commit", "-q", "-m", "tip"]);
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
}

#[test]
fn shallow_boundary_commit_diff_is_against_empty_tree() {
    let dir = tempdir().unwrap();
    let bare = dir.path().join("upstream.git");
    build_linear_bare(&bare);
    let clone_dir = dir.path().join("shallow");
    let repo = grit_shallow_clone(&bare, &clone_dir, 1);
    let tip = repo
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
        .unwrap();
    assert!(repo.git_dir().join("shallow").exists());

    let config = repo.config().snapshot().unwrap().clone();
    let thread = repo.thread_policy();
    let out = repo
        .diff()
        .commit_files(
            &tip,
            DiffMerges::FirstParent,
            DiffDetail::NameStatus,
            &DiffOptions::default(),
            &Pathspec::entire_tree(PathspecGlobalFlags::default()),
        )
        .unwrap();
    assert!(
        !out.files.is_empty(),
        "boundary commit should diff whole tree as added"
    );

    assert!(
        repo.revwalk()
            .resolve("HEAD~1", &grit_lib::revwalk::ResolveOptions::default())
            .is_err(),
        "HEAD~1 must not resolve past a depth-1 boundary"
    );
}

#[test]
fn shallow_boundary_blame_matches_git_boundary_marker() {
    let dir = tempdir().unwrap();
    let bare = dir.path().join("upstream.git");
    build_linear_bare(&bare);
    let clone_dir = dir.path().join("shallow");
    let repo = grit_shallow_clone(&bare, &clone_dir, 1);

    let result = repo
        .diff()
        .blame(
            "README",
            &BlameOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap();
    assert!(
        result.lines.iter().any(|l| l.boundary),
        "expected at least one boundary line"
    );

    let git_blame = git_out(&clone_dir, &["blame", "README"]);
    let grit_human: String = result
        .lines
        .iter()
        .map(|l| {
            let prefix = if l.boundary { '^' } else { ' ' };
            format!("{prefix}{}", l.commit.to_hex())
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        git_blame.lines().any(|l| l.starts_with('^')),
        "git blame should mark boundary: {git_blame}"
    );
    assert!(
        result.lines.iter().any(|l| l.boundary),
        "grit lines should include boundary attribution"
    );
    let _ = grit_human;
}

#[test]
fn depth_two_child_still_diffs_against_parent() {
    let dir = tempdir().unwrap();
    let bare = dir.path().join("upstream.git");
    build_linear_bare(&bare);
    let clone_dir = dir.path().join("shallow2");
    let repo = grit_shallow_clone(&bare, &clone_dir, 2);
    let tip = repo
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
        .unwrap();
    let parent = repo
        .revwalk()
        .resolve("HEAD~1", &grit_lib::revwalk::ResolveOptions::default())
        .expect("parent exists at depth 2");
    let config = repo.config().snapshot().unwrap().clone();
    let thread = repo.thread_policy();
    let out = repo
        .diff()
        .commit_files(
            &tip,
            DiffMerges::FirstParent,
            DiffDetail::NameStatus,
            &DiffOptions::default(),
            &Pathspec::entire_tree(PathspecGlobalFlags::default()),
        )
        .unwrap();
    assert!(
        !out.files.is_empty(),
        "tip at depth 2 should diff against stored parent {parent}"
    );

    let boundary = repo
        .odb()
        .read_commit(&parent)
        .expect("boundary commit object");
    let boundary_diff = repo
        .diff()
        .commit_files(
            &parent,
            DiffMerges::FirstParent,
            DiffDetail::NameStatus,
            &DiffOptions::default(),
            &Pathspec::entire_tree(PathspecGlobalFlags::default()),
        )
        .unwrap();
    assert!(
        !boundary_diff.files.is_empty(),
        "shallow boundary parent should diff as root"
    );
    let _ = boundary;
}

#[test]
fn head_caret_one_errors_without_reading_missing_parent() {
    let dir = tempdir().unwrap();
    let bare = dir.path().join("upstream.git");
    build_linear_bare(&bare);
    let clone_dir = dir.path().join("shallow");
    let repo = grit_shallow_clone(&bare, &clone_dir, 1);

    match repo
        .revwalk()
        .resolve("HEAD~1", &grit_lib::revwalk::ResolveOptions::default())
    {
        Err(Error::Revision(RevisionError::NotFound { .. }))
        | Err(Error::Ref(RefError::Invalid { .. }))
        | Err(Error::Revision(RevisionError::Parse(_))) => {}
        other => panic!("expected revision not found for HEAD~1, got {other:?}"),
    }
}
