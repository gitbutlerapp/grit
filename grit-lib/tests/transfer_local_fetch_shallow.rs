//! Shallow-clone fetch/clone via [`grit_lib::transfer::fetch_local`].
//!
//! Reproduces GitHub issue #937: fetch in a `git clone --depth 1` checkout must
//! not create tag refs to missing commits, and cloning from a shallow source must
//! propagate `.git/shallow` and stay fsck-clean with system `git`.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{fetch_local, FetchOptions, TagMode};
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

fn build_tagged_linear_source(bare: &Path) {
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git(&wt, &["add", "f"]);
        git(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
        git(&wt, &["tag", &format!("v{i}")]);
    }
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
}

#[test]
fn fetch_local_in_git_shallow_clone_does_not_create_broken_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_tagged_linear_source(&bare);

    let shallow = tmp.path().join("shallow");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", bare.display()),
            shallow.to_str().unwrap(),
        ],
    );

    let shallow_git = shallow.join(".git");
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&shallow_git, &bare, &opts).expect("fetch_local");

    assert!(
        resolve_ref(&shallow_git, "refs/tags/v1").is_err(),
        "v1 must not be created in a depth-1 shallow clone"
    );
    assert!(
        resolve_ref(&shallow_git, "refs/tags/v2").is_err(),
        "v2 must not be created in a depth-1 shallow clone"
    );
    assert_eq!(
        resolve_ref(&shallow_git, "refs/tags/v3").expect("v3"),
        rev_parse(&shallow, "HEAD")
    );

    git_fsck_clean(&shallow);
}

#[test]
fn fetch_local_clone_from_shallow_source_propagates_shallow_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_tagged_linear_source(&bare);

    let shallow_src = tmp.path().join("shallow-src");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", bare.display()),
            shallow_src.to_str().unwrap(),
        ],
    );
    assert!(
        shallow_src.join(".git/shallow").is_file(),
        "git shallow clone must have .git/shallow"
    );

    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    git(&dest, &["init", "-q", "-b", "main", "."]);
    let dest_git = dest.join(".git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        initial_remote_fetch: true,
        remote_name: Some("origin".to_owned()),
        ..Default::default()
    };
    fetch_local(&dest_git, &shallow_src.join(".git"), &opts).expect("fetch_local from shallow");

    assert!(
        dest_git.join("shallow").is_file(),
        "clone fetch must copy shallow boundaries from a shallow source"
    );
    git_fsck_clean(&dest);
}
