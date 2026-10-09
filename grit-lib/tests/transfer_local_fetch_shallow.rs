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
    assert_eq!(
        resolve_ref(&dest_git, "refs/tags/v3").expect("v3 on shallow clone"),
        rev_parse(&shallow_src, "HEAD")
    );
    assert!(
        resolve_ref(&dest_git, "refs/tags/v1").is_err(),
        "v1 must not be imported from a depth-1 source"
    );
    let log = git(&dest, &["log", "--oneline", "refs/remotes/origin/main"]);
    assert_eq!(log.lines().count(), 1, "expected single commit history");
    git_fsck_clean(&dest);
}

#[test]
fn fetch_local_initial_fetch_following_imports_reachable_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main", "."]);
    std::fs::write(upstream.join("README"), b"tag following\n").unwrap();
    git(&upstream, &["add", "README"]);
    git(&upstream, &["commit", "-q", "-m", "init"]);
    git(&upstream, &["tag", "-a", "v1.0", "-m", "release"]);
    git(&upstream, &["tag", "lightweight-tip"]);
    git(&upstream, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(upstream.join("topic.txt"), b"topic\n").unwrap();
    git(&upstream, &["add", "topic.txt"]);
    git(&upstream, &["commit", "-q", "-m", "topic work"]);
    git(&upstream, &["checkout", "-q", "main"]);
    git(
        &upstream,
        &["tag", "-a", "on-topic", "-m", "topic tag", "topic"],
    );

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let upstream_git = upstream.join(".git");

    fetch_local(
        &local_git,
        &upstream_git,
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::Following,
            initial_remote_fetch: true,
            remote_name: Some("origin".to_owned()),
            ..Default::default()
        },
    )
    .expect("initial fetch");

    for tag in [
        "refs/tags/v1.0",
        "refs/tags/lightweight-tip",
        "refs/tags/on-topic",
    ] {
        resolve_ref(&local_git, tag).unwrap_or_else(|_| panic!("missing {tag}"));
    }
    git_fsck_clean(&local);
}
