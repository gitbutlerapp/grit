//! Shallow clone (`CloneOptions::depth`) matches `git clone --depth=1` on disk.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
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
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git(&wt, &["add", "f"]);
        git(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
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

fn build_linear_bare_with_parent_tag(bare: &Path) -> ObjectId {
    let wt = bare.with_extension("wt");
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
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    parent
}

#[test]
fn clone_depth_two_keeps_annotated_tag_on_fetched_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    let parent = build_linear_bare_with_parent_tag(&bare);

    let dest = tmp.path().join("grit-shallow-d2");
    let outcome = clone_repo(
        CloneOptions {
            depth: Some(2),
            ..CloneOptions::new(bare.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
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
