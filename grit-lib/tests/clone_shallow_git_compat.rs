//! Shallow clone (`CloneOptions::depth`) matches `git clone --depth=1` on disk.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::progress::NeverCancel;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::shallow::load_shallow_oids;
use grit_test_support::git;

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn git_fsck_clean(work_tree: &Path) {
    let out = Command::new("git")
        .current_dir(work_tree)
        .args(["fsck"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn build_linear_bare(bare: &Path) {
    build_linear_bare_with_root_tag(bare, false);
}

fn build_linear_bare_with_root_tag(bare: &Path, tag_root: bool) {
    build_linear_bare_with_tags(bare, tag_root, false, false);
}

fn build_linear_bare_with_tags(bare: &Path, tag_root: bool, tag_tip: bool, tag_old: bool) {
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git(&wt, &["add", "f"]);
        git(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    if tag_root {
        git(&wt, &["tag", "-a", "root-tag", "-m", "on root", "main~2"]);
    }
    if tag_tip {
        git(&wt, &["tag", "-a", "tip-tag", "-m", "on tip"]);
    }
    if tag_old {
        git(&wt, &["tag", "-a", "old-tag", "-m", "on old", "main~1"]);
    }
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
}

fn local_tag_ref_names(git_dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/tags/"])
        .current_dir(git_dir.parent().unwrap())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("for-each-ref tags");
    assert!(out.status.success());
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

#[test]
fn clone_depth_one_records_shallow_boundary_and_limits_history() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_linear_bare(&bare);

    let tip = rev_parse(&bare, "refs/heads/main");
    let root = rev_parse(&bare, "main~2");

    let dest = tmp.path().join("grit-shallow");
    let outcome = clone_repo(
        CloneOptions {
            depth: Some(1),
            ..CloneOptions::new(bare.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("shallow clone");

    assert_eq!(outcome.checkout_oid, tip);
    assert!(dest.join(".git/shallow").is_file(), "missing .git/shallow");

    let shallow = load_shallow_oids(&dest.join(".git")).expect("load shallow");
    assert!(
        shallow.contains(&tip),
        "shallow file must graft the tip {}, got {:?}",
        tip.to_hex(),
        shallow.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );

    let commit = outcome
        .repo
        .odb()
        .read(&outcome.checkout_oid)
        .expect("commit");
    let parsed = grit_lib::objects::parse_commit(&commit.data).expect("parse commit");
    checkout_between_trees(&outcome.repo, None, &parsed.tree).expect("checkout");

    let log = git(&dest, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 1, "expected single commit: {log}");

    assert!(
        !outcome.repo.odb().exists(&root),
        "depth-1 clone must not contain deep ancestor {}",
        root.to_hex()
    );

    assert_eq!(
        resolve_ref(&dest.join(".git"), "refs/remotes/origin/main").expect("origin/main"),
        tip
    );

    git_fsck_clean(&dest);
}

fn build_main_and_side_bare(bare: &Path) {
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    std::fs::write(wt.join("f"), b"main\n").unwrap();
    git(&wt, &["add", "f"]);
    git(&wt, &["commit", "-q", "-m", "on main"]);
    git(&wt, &["checkout", "-q", "-b", "side"]);
    std::fs::write(wt.join("f"), b"side\n").unwrap();
    git(&wt, &["add", "f"]);
    git(&wt, &["commit", "-q", "-m", "on side"]);
    git(&wt, &["checkout", "-q", "main"]);
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
}

fn remote_tracking_branches(git_dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/remotes/origin/",
        ])
        .current_dir(git_dir.parent().unwrap())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("for-each-ref");
    assert!(out.status.success());
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| *l != "origin" && *l != "origin/HEAD")
        .map(|l| l.strip_prefix("origin/").unwrap_or(l).to_owned())
        .collect();
    names.sort();
    names
}

fn shallow_file_lines(git_dir: &Path) -> Vec<String> {
    let path = git_dir.join("shallow");
    if !path.is_file() {
        return Vec::new();
    }
    std::fs::read_to_string(path)
        .expect("shallow")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn shallow_clone_matches_git_single_branch_on_multi_branch_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_main_and_side_bare(&bare);
    let url = format!("file://{}", bare.display());

    let git_dest = tmp.path().join("git-clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            git_dest.to_str().unwrap(),
        ],
    );

    let grit_dest = tmp.path().join("grit-clone");
    clone_repo(
        CloneOptions {
            depth: Some(1),
            ..CloneOptions::new(url, grit_dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit shallow clone");

    let git_git = git_dest.join(".git");
    let grit_git = grit_dest.join(".git");

    assert_eq!(
        remote_tracking_branches(&git_git),
        vec!["main".to_owned()],
        "git clone --depth=1 must only create origin/main"
    );
    assert_eq!(
        remote_tracking_branches(&grit_git),
        remote_tracking_branches(&git_git),
        "grit shallow clone must match git remote-tracking branches"
    );
    assert_eq!(
        shallow_file_lines(&grit_git),
        shallow_file_lines(&git_git),
        "grit shallow boundaries must match git"
    );
    assert!(
        resolve_ref(&grit_git, "refs/remotes/origin/side").is_err(),
        "side branch must not be fetched"
    );

    git_fsck_clean(&git_dest);
    git_fsck_clean(&grit_dest);
}

fn git_fsck_strict(work_tree: &Path) {
    let out = Command::new("git")
        .current_dir(work_tree)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck --strict");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn shallow_entries_are_commits(repo: &grit_lib::repo::Repository, git_dir: &Path) -> Vec<ObjectId> {
    let text = std::fs::read_to_string(git_dir.join("shallow")).expect("shallow file");
    text.lines()
        .filter_map(|line| {
            let hex = line.trim();
            if hex.is_empty() {
                return None;
            }
            let oid = ObjectId::from_hex(hex).expect("shallow line oid");
            let kind = repo.odb().read(&oid).expect("shallow object").kind;
            assert_eq!(
                kind,
                ObjectKind::Commit,
                "shallow boundary {} must be a commit, got {:?}",
                hex,
                kind
            );
            Some(oid)
        })
        .collect()
}

#[test]
fn shallow_clone_with_root_annotated_tag_matches_git_shallow_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_linear_bare_with_root_tag(&bare, true);

    let url = format!("file://{}", bare.display());

    let git_dest = tmp.path().join("git-clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            git_dest.to_str().unwrap(),
        ],
    );

    let grit_dest = tmp.path().join("grit-clone");
    let outcome = clone_repo(
        CloneOptions {
            depth: Some(1),
            ..CloneOptions::new(url, grit_dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit shallow clone");

    assert_eq!(
        shallow_file_lines(&git_dest.join(".git")),
        shallow_file_lines(&grit_dest.join(".git")),
        "shallow file must match git clone --depth=1"
    );
    assert_eq!(
        shallow_entries_are_commits(&outcome.repo, &grit_dest.join(".git")).len(),
        1,
        "expected exactly one commit shallow boundary"
    );

    git_fsck_strict(&git_dest);
    git_fsck_strict(&grit_dest);
}

#[test]
fn shallow_clone_includes_annotated_tag_on_tip_not_on_old_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_linear_bare_with_tags(&bare, false, true, true);

    let url = format!("file://{}", bare.display());

    let git_dest = tmp.path().join("git-clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            git_dest.to_str().unwrap(),
        ],
    );

    let grit_dest = tmp.path().join("grit-clone");
    clone_repo(
        CloneOptions {
            depth: Some(1),
            ..CloneOptions::new(url, grit_dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("grit shallow clone");

    let git_git = git_dest.join(".git");
    let grit_git = grit_dest.join(".git");

    assert_eq!(
        local_tag_ref_names(&git_git),
        local_tag_ref_names(&grit_git),
        "tag refs must match git clone --depth=1"
    );
    assert_eq!(
        shallow_file_lines(&grit_git),
        shallow_file_lines(&git_git),
        "shallow file must match git"
    );
    assert!(
        local_tag_ref_names(&grit_git).contains(&"tip-tag".to_owned()),
        "tip annotated tag must be present"
    );
    assert!(
        !local_tag_ref_names(&grit_git).contains(&"old-tag".to_owned()),
        "tag on unfetched commit must be absent"
    );

    git_fsck_strict(&git_dest);
    git_fsck_strict(&grit_dest);
}

#[test]
fn shallow_depth_two_keeps_annotated_tag_on_fetched_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git(&wt, &["add", "f"]);
        git(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    let parent = rev_parse(&wt, "HEAD~1");
    git(
        &wt,
        &[
            "tag",
            "-a",
            "parent-tag",
            "-m",
            "on parent",
            &parent.to_hex(),
        ],
    );
    let bare = tmp.path().join("remote.git");
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);

    let dest = tmp.path().join("grit-shallow-d2");
    let outcome = clone_repo(
        CloneOptions {
            depth: Some(2),
            ..CloneOptions::new(bare.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("depth-2 shallow clone");

    let tag_oid = resolve_ref(&dest.join(".git"), "refs/tags/parent-tag")
        .expect("refs/tags/parent-tag must exist like git clone --depth=2");
    assert!(
        outcome
            .repo
            .odb()
            .peel_tags(tag_oid)
            .is_ok_and(|(peeled, _)| peeled == parent),
        "tag must peel to parent commit {}",
        parent.to_hex()
    );

    let git_dest = tmp.path().join("git-shallow-d2");
    let url = format!("file://{}", bare.to_string_lossy());
    let status = Command::new("git")
        .args([
            "clone",
            "-q",
            "--depth",
            "2",
            &url,
            git_dest.to_str().unwrap(),
        ])
        .status()
        .expect("git clone");
    assert!(status.success(), "git clone --depth=2 failed");
    let git_tag = resolve_ref(&git_dest.join(".git"), "refs/tags/parent-tag")
        .expect("git must keep parent-tag");
    assert_eq!(tag_oid, git_tag, "grit and git must agree on tag oid");
}
